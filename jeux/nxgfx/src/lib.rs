//! nxgfx: what a game needs from the console. `Graphics::open` takes the
//! screen from the UI, powers the GPU and sets up the images to draw on;
//! a `Frame` records a command stream that `present` runs on the GPU and
//! then shows at the next vertical blank; `poll` returns the input the
//! console routes to the game (touch, gamepad).
//!
//! A game keeps its saves in the directory the console gives it as
//! `NXK_SAVE_DIR` (one per user), never under `/games`.
//!
//! The panel is portrait: the landscape picture (1280 x 720, the one the
//! UI shows) is the image turned a quarter. The projection does the
//! turn, at no cost, through `Graphics::panel`.

#![no_std]

extern crate alloc;

pub mod audio;
pub mod canvas;
pub mod math;
pub mod os;
pub mod shaders;

use alloc::vec::Vec;

use nxk_abi::proto::{display, gpu, input};
use nxk_abi::shm;
use nxk_abi::user::Msg;

pub use audio::{Audio, Sound};
pub use canvas::{rgb, Canvas, Rgba};
pub use math::{vec3, Mat4, Vec3};
pub use nxk_shader as ir;
pub use os::{now_ns, sleep_ns, Result};
pub use shaders::{
    Attr, Blend, Buffer, Cull, DepthFunc, Fullscreen, Pipeline, PipelineDesc, Shader,
};

/// What failed while opening the console's services, and the errno.
#[derive(Clone, Copy, Debug)]
pub struct OpenError {
    pub step: &'static str,
    pub errno: i64,
}

impl core::fmt::Display for OpenError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{}: errno {}", self.step, -self.errno)
    }
}

fn at<T>(step: &'static str, r: Result<T>) -> core::result::Result<T, OpenError> {
    r.map_err(|errno| OpenError { step, errno })
}

use os::{received_handle, Service, Shm};

/// Landscape size of the picture.
pub const WIDTH: u32 = 1280;
pub const HEIGHT: u32 = 720;
/// Bytes of the command stream of a frame.
const CMD_BYTES: usize = 1 << 20;
/// Vertices of a canvas: 32 bytes each.
const CANVAS_BYTES: usize = 2 << 20;

/// Gamepad buttons (`input::BTN_*`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Button {
    A,
    B,
    X,
    Y,
    L,
    R,
    Zl,
    Zr,
    Plus,
    Minus,
    Up,
    Down,
    Left,
    Right,
    LStick,
    RStick,
    Home,
    Capture,
}

impl Button {
    fn from_code(c: u64) -> Option<Button> {
        use Button::*;
        Some(match c {
            input::BTN_A => A,
            input::BTN_B => B,
            input::BTN_X => X,
            input::BTN_Y => Y,
            input::BTN_L => L,
            input::BTN_R => R,
            input::BTN_ZL => Zl,
            input::BTN_ZR => Zr,
            input::BTN_PLUS => Plus,
            input::BTN_MINUS => Minus,
            input::BTN_UP => Up,
            input::BTN_DOWN => Down,
            input::BTN_LEFT => Left,
            input::BTN_RIGHT => Right,
            input::BTN_LSTICK => LStick,
            input::BTN_RSTICK => RStick,
            input::BTN_HOME => Home,
            input::BTN_CAPTURE => Capture,
            _ => return None,
        })
    }
}

/// Analog sticks (`input::STICK_*`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stick {
    Left,
    Right,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    Down,
    Move,
    Up,
}

/// Input from the console.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Event {
    /// A finger, at landscape pixel (x, y).
    Touch {
        phase: Phase,
        finger: u32,
        x: i32,
        y: i32,
    },
    Button {
        button: Button,
        down: bool,
    },
    /// A stick moved: x to the right and y up, -1.0 to 1.0, both 0.0 at
    /// rest (the console's dead zone is taken off).
    Stick {
        stick: Stick,
        x: f32,
        y: f32,
    },
}

/// Raw touch panel range to the landscape picture
/// (`display`'s own mapping).
fn touch_xy(x: u64, y: u64) -> (i32, i32) {
    let e = input::TOUCH_EDGE;
    let px = (x.clamp(e, input::TOUCH_X_MAX) - e) * WIDTH as u64 / (input::TOUCH_X_MAX - e);
    let py = (y.clamp(e, input::TOUCH_Y_MAX) - e) * HEIGHT as u64 / (input::TOUCH_Y_MAX - e);
    (px as i32, py as i32)
}

/// Vertex of `Pipe::Lit`.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct LitVertex {
    pub pos: [f32; 3],
    pub normal: [f32; 3],
    pub color: [u8; 4],
}

/// Vertex of `Pipe::Textured`: `LitVertex` with texture coordinates after
/// the normal. (0, 0) is the first row and column of the image.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct TexVertex {
    pub pos: [f32; 3],
    pub normal: [f32; 3],
    pub uv: [f32; 2],
    pub color: [u8; 4],
}

/// Vertex of `Pipe::Flat`: clip-space position and color.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct FlatVertex {
    pub pos: [f32; 4],
    pub color: [f32; 4],
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pipe {
    Flat,
    Lit,
    /// `Lit`, with the color times the texel of the texture bound by
    /// `Frame::texture`; the uniforms are `LitUniforms`.
    Textured,
}

/// How a texture is sampled: linear filtering and repeat by default.
#[derive(Clone, Copy, Debug, Default)]
pub struct Sampling {
    /// Nearest texel rather than a blend of four.
    pub nearest: bool,
    /// Coordinates outside 0..1 take the edge texel rather than repeat.
    pub clamp: bool,
}

impl Sampling {
    fn flags(self) -> u64 {
        let mut f = 0;
        if self.nearest {
            f |= gpu::TEX_NEAREST;
        }
        if self.clamp {
            f |= gpu::TEX_CLAMP;
        }
        f
    }
}

/// What a texel or a pixel of a render target holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TexFormat {
    /// Four bytes (R, G, B, A), 0..255 read as 0..1.
    Rgba8,
    /// One 32-bit float (a sampler reads (r, 0, 0, 1)): shadow maps.
    R32f,
    /// Four 16-bit floats: HDR color.
    Rgba16f,
    /// Four 32-bit floats.
    Rgba32f,
}

impl TexFormat {
    fn code(self) -> u64 {
        match self {
            TexFormat::Rgba8 => gpu::TF_RGBA8,
            TexFormat::R32f => gpu::TF_R32F,
            TexFormat::Rgba16f => gpu::TF_RGBA16F,
            TexFormat::Rgba32f => gpu::TF_RGBA32F,
        }
    }

    /// Bytes per pixel.
    pub fn bytes(self) -> u32 {
        gpu::tf_bytes(self.code())
    }
}

/// An image the GPU samples (`Graphics::texture`, `RenderTarget::texture`).
pub struct Texture {
    _mem: Option<Shm>,
    id: u64,
    pub width: u32,
    pub height: u32,
    pub format: TexFormat,
}

/// An image to draw on, which the GPU can sample afterwards (a shadow
/// map, an HDR buffer for post-processing), with its own depth buffer if
/// asked. `Frame::target` draws on it; the passes that follow can sample
/// `texture()`.
pub struct RenderTarget {
    _mem: Shm,
    _depth_mem: Option<Shm>,
    color: u64,
    depth: u32,
    texture: Texture,
}

impl RenderTarget {
    /// The image as a texture, for `Frame::bind_texture` / `Frame::texture`.
    pub fn texture(&self) -> &Texture {
        &self.texture
    }

    pub fn width(&self) -> u32 {
        self.texture.width
    }

    pub fn height(&self) -> u32 {
        self.texture.height
    }
}

/// Parameters of `Pipe::Lit`: the shaders compute `mvp * position`,
/// light a normal turned by `model` (its rotation), with one directional
/// light shining from `to_light` (unit vector, in the same space).
#[derive(Clone, Copy, Debug)]
pub struct LitUniforms {
    pub mvp: Mat4,
    pub model: Mat4,
    pub to_light: Vec3,
    pub ambient: f32,
    pub diffuse: f32,
}

impl LitUniforms {
    fn words(&self) -> [u32; gpu::LIT_UBO_WORDS] {
        let mut w = [0u32; gpu::LIT_UBO_WORDS];
        for r in 0..4 {
            for c in 0..4 {
                w[4 * r + c] = self.mvp.0[r][c].to_bits();
            }
        }
        for r in 0..3 {
            for c in 0..3 {
                w[16 + 4 * r + c] = self.model.0[r][c].to_bits();
            }
        }
        w[28] = self.to_light.x.to_bits();
        w[29] = self.to_light.y.to_bits();
        w[30] = self.to_light.z.to_bits();
        w[31] = self.ambient.to_bits();
        w[32] = self.diffuse.to_bits();
        w
    }
}

/// The next mip level of a `w` x `h` RGBA8 image (rows from the top): each
/// texel the rounded average of the 2 x 2 above it (an odd edge repeats).
fn halve(src: &[u8], w: u32, h: u32) -> Vec<u8> {
    let (nw, nh) = ((w / 2).max(1), (h / 2).max(1));
    let mut out = alloc::vec![0u8; nw as usize * nh as usize * 4];
    for y in 0..nh {
        for x in 0..nw {
            let (x0, y0) = ((2 * x).min(w - 1), (2 * y).min(h - 1));
            let (x1, y1) = ((2 * x + 1).min(w - 1), (2 * y + 1).min(h - 1));
            for c in 0..4 {
                let t =
                    |px: u32, py: u32| src[(py as usize * w as usize + px as usize) * 4 + c] as u32;
                let sum = t(x0, y0) + t(x1, y0) + t(x0, y1) + t(x1, y1);
                out[(y as usize * nw as usize + x as usize) * 4 + c] = ((sum + 2) / 4) as u8;
            }
        }
    }
    out
}

/// Vertices and indices in GPU-visible memory.
pub struct Mesh {
    _vertices: Shm,
    _indices: Shm,
    vb: u64,
    ib: u64,
    pub vertex_count: u32,
    pub index_count: u32,
}

struct Target {
    id: u64,
    _mem: Option<Shm>,
}

pub struct Graphics {
    display: Service,
    gpu: Service,
    /// Images on the panel, scanned out in turn; the one on display.
    images: Vec<Target>,
    shown: usize,
    depth: Target,
    cmd: Shm,
    cmd_id: u64,
    /// Memory for a canvas, rewritten every frame (the GPU is idle then).
    canvas: Shm,
    canvas_id: u64,
    canvas_used: usize,
    words: Vec<u32>,
    /// Frames presented.
    pub frames: u64,
    started: u64,
    open: bool,
}

fn word(m: &Msg, i: usize) -> u64 {
    m.mr[i]
}

impl Graphics {
    /// Power the GPU, take the screen. `ENODEV`: another program has it.
    pub fn open() -> core::result::Result<Self, OpenError> {
        let display = at("display service", Service::lookup("display"))?;
        Self::open_with(display).inspect_err(|_| {
            // The screen may be ours already: give it back.
            if let Ok(d) = Service::lookup("display") {
                let _ = d.call(&Msg::new(display::RELEASE, 0, 0));
            }
        })
    }

    fn open_with(display: Service) -> core::result::Result<Self, OpenError> {
        let gpu_s = at("gpu service", Service::lookup("gpu"))?;
        let st = at("gpu power", gpu_s.call(&Msg::from_words(gpu::POWER, &[1])))?;
        if word(&st, 0) & 0xFF != gpu::ON {
            return Err(OpenError {
                step: "gpu power (failed)",
                errno: -nxk_abi::err::EIO,
            });
        }
        // The highest clock the power source allows (battery or charger).
        for &khz in gpu::STEPS_KHZ.iter().rev() {
            if gpu_s.call(&Msg::from_words(gpu::CLOCK, &[khz, 0])).is_ok() {
                break;
            }
        }
        let r = at("screen", display.call(&Msg::new(display::SCANOUT, 0, 0)))?;
        let handles: Vec<u32> = (0..display::SCANOUT_BUFFERS).map(received_handle).collect();
        let (w, h, bl) = (word(&r, 0) as u32, word(&r, 1) as u32, word(&r, 3));
        let mut images = Vec::new();
        let mut failed = None;
        for &hd in &handles {
            let m = Msg::from_words(
                gpu::TARGET_ATTACH,
                &[w as u64 | (h as u64) << 32, gpu::TARGET_COLOR, bl],
            );
            match gpu_s.call_with(m, hd) {
                Ok(a) => images.push(Target {
                    id: word(&a, 0),
                    _mem: None,
                }),
                Err(e) => failed = Some(e),
            }
            os::handle_close(hd);
        }
        if let Some(e) = failed {
            let _ = display.call(&Msg::new(display::RELEASE, 0, 0));
            return Err(OpenError {
                step: "color target",
                errno: e,
            });
        }
        let dbytes = gpu::tiled_bytes(w, h, gpu::DEPTH_BLOCK_LOG2);
        let dmem = at("depth memory", Shm::new(dbytes, 0))?;
        let m = Msg::from_words(
            gpu::TARGET_ATTACH,
            &[w as u64 | (h as u64) << 32, gpu::TARGET_DEPTH, 0],
        );
        let d = at("depth target", gpu_s.call_with(m, dmem.handle()))?;
        let depth = Target {
            id: word(&d, 0),
            _mem: Some(dmem),
        };
        let cmd = at("command memory", Shm::new(CMD_BYTES, 0))?;
        let m = Msg::from_words(gpu::BUFFER_ATTACH, &[CMD_BYTES as u64]);
        let c = at("command buffer", gpu_s.call_with(m, cmd.handle()))?;
        let cvs = at("canvas memory", Shm::new(CANVAS_BYTES, shm::UNCACHED))?;
        let m = Msg::from_words(gpu::BUFFER_ATTACH, &[CANVAS_BYTES as u64]);
        let cid = word(&at("canvas buffer", gpu_s.call_with(m, cvs.handle()))?, 0);
        Ok(Self {
            display,
            gpu: gpu_s,
            images,
            shown: 0,
            depth,
            cmd,
            cmd_id: word(&c, 0),
            canvas: cvs,
            canvas_id: cid,
            canvas_used: 0,
            words: Vec::with_capacity(4096),
            frames: 0,
            started: now_ns(),
            open: true,
        })
    }

    /// The panel turns the landscape picture a quarter: multiply a
    /// projection by this to draw upright.
    pub fn panel() -> Mat4 {
        Mat4([
            [0.0, -1.0, 0.0, 0.0],
            [-1.0, 0.0, 0.0, 0.0],
            [0.0, 0.0, 1.0, 0.0],
            [0.0, 0.0, 0.0, 1.0],
        ])
    }

    /// Projection for the landscape picture, upright on the panel.
    pub fn projection(fov_y: f32, near: f32, far: f32) -> Mat4 {
        Self::panel() * Mat4::perspective(fov_y, WIDTH as f32 / HEIGHT as f32, near, far)
    }

    pub(crate) fn buffer_bytes(&self, data: &[u8]) -> Result<(Shm, u64)> {
        self.buffer(data)
    }

    fn buffer(&self, data: &[u8]) -> Result<(Shm, u64)> {
        let shm = Shm::new(data.len().max(4), shm::UNCACHED)?;
        shm.write(0, data);
        let m = Msg::from_words(gpu::BUFFER_ATTACH, &[shm.len() as u64]);
        let id = word(&self.gpu.call_with(m, shm.handle())?, 0);
        Ok((shm, id))
    }

    pub fn mesh_lit(&self, vertices: &[LitVertex], indices: &[u16]) -> Result<Mesh> {
        self.mesh(vertices, indices)
    }

    pub fn mesh_textured(&self, vertices: &[TexVertex], indices: &[u16]) -> Result<Mesh> {
        self.mesh(vertices, indices)
    }

    pub fn mesh_flat(&self, vertices: &[FlatVertex], indices: &[u16]) -> Result<Mesh> {
        self.mesh(vertices, indices)
    }

    /// An image for `Frame::texture`: `rgba` is `width` x `height` pixels,
    /// four bytes each (R, G, B, A), rows from the top. The GPU wants
    /// them in its block-linear layout: this swizzles them.
    pub fn texture(
        &self,
        width: u32,
        height: u32,
        rgba: &[u8],
        sampling: Sampling,
    ) -> Result<Texture> {
        self.texture_levels(width, height, rgba, sampling, false)
    }

    /// `texture` with a chain of mip levels down to 1 x 1, each the
    /// average of 2 x 2 texels of the one before (made here, on the CPU):
    /// the GPU blends two levels by the size of the pixel on the texture
    /// (trilinear; the nearest level with `Sampling::nearest`), so far
    /// ground and small objects stay clean.
    pub fn texture_mipped(
        &self,
        width: u32,
        height: u32,
        rgba: &[u8],
        sampling: Sampling,
    ) -> Result<Texture> {
        self.texture_levels(width, height, rgba, sampling, true)
    }

    fn texture_levels(
        &self,
        width: u32,
        height: u32,
        rgba: &[u8],
        sampling: Sampling,
        mips: bool,
    ) -> Result<Texture> {
        if width == 0
            || height == 0
            || width > 8192
            || height > 8192
            || rgba.len() != width as usize * height as usize * 4
        {
            return Err(-nxk_abi::err::EINVAL);
        }
        let bl = gpu::tex_block_log2(height);
        let levels = if mips {
            gpu::max_levels(width, height)
        } else {
            1
        };
        let bytes = gpu::mip_bytes(width, height, 4, bl, levels);
        let mut tiled = alloc::vec![0u8; bytes];
        let mut cur = rgba.to_vec();
        for level in 0..levels {
            let (w, h) = (
                gpu::level_extent(width, level),
                gpu::level_extent(height, level),
            );
            let base = gpu::level_offset(width, height, 4, bl, level);
            let lbl = gpu::level_block_log2(bl, h);
            for y in 0..h {
                for x in 0..w {
                    let at = base + gpu::tiled_offset(w, x, y, 4, lbl);
                    let from = (y as usize * w as usize + x as usize) * 4;
                    tiled[at..at + 4].copy_from_slice(&cur[from..from + 4]);
                }
            }
            if level + 1 < levels {
                cur = halve(&cur, w, h);
            }
        }
        let mem = Shm::new(bytes, shm::UNCACHED)?;
        mem.write(0, &tiled);
        let m = Msg::from_words(
            gpu::TEXTURE_ATTACH,
            &[
                width as u64 | (height as u64) << 32,
                sampling.flags(),
                gpu::TF_RGBA8,
                0,
                levels as u64,
            ],
        );
        let id = word(&self.gpu.call_with(m, mem.handle())?, 0);
        Ok(Texture {
            _mem: Some(mem),
            id,
            width,
            height,
            format: TexFormat::Rgba8,
        })
    }

    /// An image to draw on and then sample, `width` x `height` pixels of
    /// `format`, with a depth buffer if `depth`. Its texture is sampled
    /// as `sampling` says (clamped is what a shadow map or a post-process
    /// buffer wants).
    pub fn render_target(
        &self,
        width: u32,
        height: u32,
        format: TexFormat,
        depth: bool,
        sampling: Sampling,
    ) -> Result<RenderTarget> {
        if width == 0 || height == 0 || width > 8192 || height > 8192 {
            return Err(-nxk_abi::err::EINVAL);
        }
        let bl = gpu::tex_block_log2(height);
        let bytes = gpu::tiled_bytes_bpp(width, height, format.bytes(), bl);
        let mem = Shm::new(bytes, shm::UNCACHED)?;
        let size = width as u64 | (height as u64) << 32;
        let m = Msg::from_words(
            gpu::TARGET_ATTACH,
            &[size, gpu::TARGET_COLOR, bl as u64, format.code()],
        );
        let color = word(&self.gpu.call_with(m, mem.handle())?, 0);
        // The same memory as a texture: same format and block height.
        let m = Msg::from_words(
            gpu::TEXTURE_ATTACH,
            &[size, sampling.flags(), format.code(), bl as u64 + 1],
        );
        let id = word(&self.gpu.call_with(m, mem.handle())?, 0);
        let (depth_mem, depth_id) = if depth {
            let dmem = Shm::new(
                gpu::tiled_bytes(width, height, gpu::DEPTH_BLOCK_LOG2),
                shm::UNCACHED,
            )?;
            let m = Msg::from_words(gpu::TARGET_ATTACH, &[size, gpu::TARGET_DEPTH, 0]);
            let d = word(&self.gpu.call_with(m, dmem.handle())?, 0);
            (Some(dmem), d as u32)
        } else {
            (None, gpu::NO_TARGET)
        };
        Ok(RenderTarget {
            _mem: mem,
            _depth_mem: depth_mem,
            color,
            depth: depth_id,
            texture: Texture {
                _mem: None,
                id,
                width,
                height,
                format,
            },
        })
    }

    /// A mesh of vertices of any layout (the pipeline's), 16-bit indices.
    pub fn mesh_raw<T: Copy>(&self, vertices: &[T], indices: &[u16]) -> Result<Mesh> {
        self.mesh(vertices, indices)
    }

    fn mesh<T: Copy>(&self, vertices: &[T], indices: &[u16]) -> Result<Mesh> {
        let vb = unsafe {
            core::slice::from_raw_parts(
                vertices.as_ptr() as *const u8,
                core::mem::size_of_val(vertices),
            )
        };
        let ib = unsafe {
            core::slice::from_raw_parts(
                indices.as_ptr() as *const u8,
                core::mem::size_of_val(indices),
            )
        };
        let (vs, vid) = self.buffer(vb)?;
        let (is, iid) = self.buffer(ib)?;
        Ok(Mesh {
            _vertices: vs,
            _indices: is,
            vb: vid,
            ib: iid,
            vertex_count: vertices.len() as u32,
            index_count: indices.len() as u32,
        })
    }

    /// Seconds since the program started.
    pub fn seconds(&self) -> f32 {
        (now_ns() - self.started) as f32 / 1e9
    }

    /// A new frame, drawing on the image that is not on display.
    pub fn frame(&mut self) -> Frame<'_> {
        self.words.clear();
        self.canvas_used = 0;
        let back = (self.shown + 1) % self.images.len();
        let (c, z) = (self.images[back].id as u32, self.depth.id as u32);
        let mut f = Frame { gfx: self, back };
        f.push(gpu::CMD_TARGET, &[c, z]);
        f
    }

    /// The next input event, waiting at most `timeout_ns` (0: for good).
    pub fn poll(&self, timeout_ns: u64) -> Option<Event> {
        let r = self
            .display
            .call(&Msg::from_words(display::EVENT, &[timeout_ns]))
            .ok()?;
        let (kind, code, value, x, y) = (
            word(&r, 0),
            word(&r, 1),
            word(&r, 2),
            word(&r, 3),
            word(&r, 4),
        );
        match kind {
            input::KIND_TOUCH => {
                let phase = match code {
                    input::TOUCH_DOWN => Phase::Down,
                    input::TOUCH_MOVE => Phase::Move,
                    _ => Phase::Up,
                };
                let (px, py) = touch_xy(x, y);
                Some(Event::Touch {
                    phase,
                    finger: value as u32,
                    x: px,
                    y: py,
                })
            }
            input::KIND_BUTTON => Some(Event::Button {
                button: Button::from_code(code)?,
                down: value != 0,
            }),
            input::KIND_STICK => {
                let axis = |w: u64| (w as i64 as f32 / input::STICK_MAX as f32).clamp(-1.0, 1.0);
                Some(Event::Stick {
                    stick: match code {
                        input::STICK_L => Stick::Left,
                        input::STICK_R => Stick::Right,
                        _ => return None,
                    },
                    x: axis(x),
                    y: axis(y),
                })
            }
            _ => None,
        }
    }

    /// Give the screen back to the UI.
    pub fn close(&mut self) {
        if self.open {
            self.open = false;
            let _ = self.display.call(&Msg::new(display::RELEASE, 0, 0));
            let _ = self.gpu.call(&Msg::new(gpu::RESET, 0, 0));
        }
    }
}

impl Drop for Graphics {
    fn drop(&mut self) {
        self.close();
    }
}

/// A frame being recorded.
pub struct Frame<'a> {
    gfx: &'a mut Graphics,
    back: usize,
}

impl Frame<'_> {
    fn push(&mut self, op: u32, args: &[u32]) {
        self.gfx.words.push(op | (args.len() as u32) << 16);
        self.gfx.words.extend_from_slice(args);
    }

    /// Draw on `t` (and its depth buffer) from here on, after the passes
    /// before are done: they may have drawn what the next ones sample.
    /// Choose the pipeline again: a draw needs one set for this target.
    pub fn target(&mut self, t: &RenderTarget) {
        self.push(gpu::CMD_TARGET, &[t.color as u32, t.depth]);
    }

    /// Draw on the screen's image again (the one `frame` began with).
    pub fn screen(&mut self) {
        let (c, z) = (
            self.gfx.images[self.back].id as u32,
            self.gfx.depth.id as u32,
        );
        self.push(gpu::CMD_TARGET, &[c, z]);
    }

    /// Color and depth to `rgba` and 1.0.
    pub fn clear(&mut self, rgba: [f32; 4]) {
        self.push(gpu::CMD_CLEAR, &rgba.map(f32::to_bits));
    }

    pub fn pipeline(&mut self, pipe: Pipe, cull_back: bool) {
        let p = match pipe {
            Pipe::Flat => gpu::PIPE_FLAT,
            Pipe::Lit => gpu::PIPE_LIT,
            Pipe::Textured => gpu::PIPE_TEXTURED,
        };
        self.push(
            gpu::CMD_PIPELINE,
            &[p, if cull_back { gpu::PF_CULL_BACK } else { 0 }],
        );
    }

    /// The texture `Pipe::Textured` draws sample, from here on.
    pub fn texture(&mut self, t: &Texture) {
        self.push(gpu::CMD_TEXTURE, &[t.id as u32]);
    }

    /// Constant buffer data for the draws that follow with `Pipe::Lit` or
    /// `Pipe::Textured`.
    pub fn lit(&mut self, u: &LitUniforms) {
        self.push(gpu::CMD_UNIFORMS, &u.words());
    }

    /// Draw a canvas: flat triangles in front of the scene.
    pub fn canvas(&mut self, c: &Canvas) {
        let at = self.gfx.canvas_used;
        let n = c.verts.len().min((CANVAS_BYTES - at) / 32) / 3 * 3;
        if n == 0 {
            return;
        }
        let bytes = unsafe {
            core::slice::from_raw_parts(
                c.verts.as_ptr() as *const u8,
                n * core::mem::size_of::<FlatVertex>(),
            )
        };
        self.gfx.canvas.write(at, bytes);
        self.gfx.canvas_used = at + n * 32;
        self.pipeline(Pipe::Flat, false);
        self.push(
            gpu::CMD_DRAW,
            &[self.gfx.canvas_id as u32, at as u32, n as u32, 0, n as u32],
        );
    }

    pub fn draw(&mut self, m: &Mesh) {
        self.push(
            gpu::CMD_DRAW_INDEXED,
            &[
                m.vb as u32,
                0,
                m.vertex_count,
                m.ib as u32,
                0,
                m.index_count,
                0,
                m.index_count,
            ],
        );
    }

    /// The shaders and state of `p` for the draws that follow.
    pub fn use_pipeline(&mut self, p: &Pipeline) {
        self.push(gpu::CMD_USE_PIPELINE, &[p.id as u32]);
    }

    /// Buffer `slot` (0-7) of the shaders' `load` / `store` instructions.
    pub fn bind_buffer(&mut self, slot: u32, b: &Buffer) {
        self.push(gpu::CMD_BIND_BUFFER, &[slot, b.id as u32]);
    }

    /// Texture `slot` (0-15) of the shaders' `sample` instructions.
    pub fn bind_texture(&mut self, slot: u32, t: &Texture) {
        self.push(gpu::CMD_BIND_TEXTURE, &[slot, t.id as u32]);
    }

    /// The shaders' uniforms (up to 1024 words) from here on.
    pub fn uniforms(&mut self, words: &[u32]) {
        self.push(gpu::CMD_UNIFORMS, words);
    }

    pub fn uniforms_f32(&mut self, v: &[f32]) {
        let w: Vec<u32> = v.iter().map(|x| x.to_bits()).collect();
        self.push(gpu::CMD_UNIFORMS, &w);
    }

    /// Run a compute shader over `groups` groups of its size, after
    /// everything recorded before it; what follows sees its results.
    pub fn dispatch(&mut self, s: &Shader, groups: [u32; 3]) {
        self.push(
            gpu::CMD_DISPATCH,
            &[s.id as u32, groups[0], groups[1], groups[2]],
        );
    }

    /// Everything before is done when what follows starts (a pass that
    /// draws something a later one samples).
    pub fn barrier(&mut self) {
        self.push(gpu::CMD_BARRIER, &[]);
    }

    /// Triangles from buffers: `vertices` of the pipeline's stride and
    /// 16-bit `indices`, `first` to `first + count` of them.
    pub fn draw_buffers(&mut self, vb: &Buffer, vertices: u32, ib: &Buffer, indices: u32) {
        self.push(
            gpu::CMD_DRAW_INDEXED,
            &[
                vb.id as u32,
                0,
                vertices,
                ib.id as u32,
                0,
                indices,
                0,
                indices,
            ],
        );
    }

    /// Triangles from a buffer of vertices, no indices: `vertices` of the
    /// pipeline's stride, drawn in threes.
    pub fn draw_vertices(&mut self, vb: &Buffer, vertices: u32) {
        self.push(gpu::CMD_DRAW, &[vb.id as u32, 0, vertices, 0, vertices]);
    }

    /// A pass over every pixel of the target: `f`'s triangle, with the
    /// pipeline of `Fullscreen::pipeline_desc` in use.
    pub fn draw_fullscreen(&mut self, f: &Fullscreen) {
        self.draw_vertices(&f.vb, 3);
    }

    /// `draw_buffers` once for each of `instances` instances (1 to
    /// 4096): the shader sees the instance number as `InstanceId`, and
    /// reads the pipeline's instance attributes from `data`, one
    /// `PipelineDesc::instance_stride` of it per instance (none if the
    /// pipeline has no instance attributes). Custom pipelines only.
    pub fn draw_instanced(
        &mut self,
        vb: &Buffer,
        vertices: u32,
        ib: &Buffer,
        indices: u32,
        data: Option<&Buffer>,
        instances: u32,
    ) {
        let d = data.map_or(gpu::NO_TARGET, |b| b.id as u32);
        self.push(
            gpu::CMD_DRAW_INDEXED_INSTANCED,
            &[
                vb.id as u32,
                0,
                vertices,
                ib.id as u32,
                0,
                indices,
                0,
                indices,
                d,
                0,
                instances,
            ],
        );
    }

    /// `draw_vertices` for each of `instances` instances, as `draw_instanced`.
    pub fn draw_vertices_instanced(
        &mut self,
        vb: &Buffer,
        vertices: u32,
        data: Option<&Buffer>,
        instances: u32,
    ) {
        let d = data.map_or(gpu::NO_TARGET, |b| b.id as u32);
        self.push(
            gpu::CMD_DRAW_INSTANCED,
            &[vb.id as u32, 0, vertices, 0, vertices, d, 0, instances],
        );
    }

    /// Run it on the GPU, then show it from the next vertical blank (the
    /// call returns then: the pace of the loop is the screen's).
    pub fn present(self) -> Result<()> {
        let g = self.gfx;
        let n = g.words.len();
        if n * 4 > g.cmd.len() {
            return Err(-nxk_abi::err::ENOSPC);
        }
        g.cmd.write_words(0, &g.words);
        g.gpu
            .call(&Msg::from_words(gpu::RENDER, &[g.cmd_id, 0, n as u64]))?;
        g.display
            .call(&Msg::from_words(display::FLIP, &[self.back as u64]))?;
        g.shown = self.back;
        g.frames += 1;
        Ok(())
    }
}

/// Unit cube centered on the origin, faces of one color each (`colors`:
/// +x, -x, +y, -y, +z, -z), normals out, counter-clockwise seen from out.
pub fn cube(size: f32, colors: [[u8; 4]; 6]) -> (Vec<LitVertex>, Vec<u16>) {
    let h = size / 2.0;
    // Face: normal, then the corners in counter-clockwise order.
    let faces: [([f32; 3], [[f32; 3]; 4]); 6] = [
        (
            [1.0, 0.0, 0.0],
            [[h, -h, h], [h, -h, -h], [h, h, -h], [h, h, h]],
        ),
        (
            [-1.0, 0.0, 0.0],
            [[-h, -h, -h], [-h, -h, h], [-h, h, h], [-h, h, -h]],
        ),
        (
            [0.0, 1.0, 0.0],
            [[-h, h, h], [h, h, h], [h, h, -h], [-h, h, -h]],
        ),
        (
            [0.0, -1.0, 0.0],
            [[-h, -h, -h], [h, -h, -h], [h, -h, h], [-h, -h, h]],
        ),
        (
            [0.0, 0.0, 1.0],
            [[-h, -h, h], [h, -h, h], [h, h, h], [-h, h, h]],
        ),
        (
            [0.0, 0.0, -1.0],
            [[h, -h, -h], [-h, -h, -h], [-h, h, -h], [h, h, -h]],
        ),
    ];
    let mut v = Vec::new();
    let mut i = Vec::new();
    for (f, (n, corners)) in faces.iter().enumerate() {
        let base = v.len() as u16;
        for c in corners {
            v.push(LitVertex {
                pos: *c,
                normal: *n,
                color: colors[f],
            });
        }
        i.extend_from_slice(&[base, base + 1, base + 2, base, base + 2, base + 3]);
    }
    (v, i)
}

/// Flat-shaded mesh from triangles (each with a color): the normals are
/// computed and turned away from `center`, so winding does not matter.
pub fn tris_mesh(tris: &[([Vec3; 3], [u8; 4])], center: Vec3) -> (Vec<LitVertex>, Vec<u16>) {
    let mut v = Vec::new();
    let mut i = Vec::new();
    for (p, color) in tris {
        let mut n = (p[1] - p[0]).cross(p[2] - p[0]).normalized();
        let mid = (p[0] + p[1] + p[2]) * (1.0 / 3.0);
        if n.dot(mid - center) < 0.0 {
            n = -n;
        }
        let base = v.len() as u16;
        for q in p {
            v.push(LitVertex {
                pos: [q.x, q.y, q.z],
                normal: [n.x, n.y, n.z],
                color: *color,
            });
        }
        i.extend_from_slice(&[base, base + 1, base + 2]);
    }
    (v, i)
}

/// Octahedron of radius `r`, one color.
pub fn octahedron(r: f32, color: [u8; 4]) -> (Vec<LitVertex>, Vec<u16>) {
    let (px, nx) = (vec3(r, 0.0, 0.0), vec3(-r, 0.0, 0.0));
    let (py, ny) = (vec3(0.0, r, 0.0), vec3(0.0, -r, 0.0));
    let (pz, nz) = (vec3(0.0, 0.0, r), vec3(0.0, 0.0, -r));
    let mut t = Vec::new();
    for (a, b) in [(px, pz), (pz, nx), (nx, nz), (nz, px)] {
        t.push(([py, a, b], color));
        t.push(([ny, b, a], color));
    }
    tris_mesh(&t, Vec3::default())
}

/// `ground` with texture coordinates: `texels_per_unit` repeats of the
/// image per world unit, and a color from `near_color` at `z_near` to
/// `far_color` at `z_far` (the texel is multiplied by it: a fade to dark
/// hides the shimmer of a far-off pattern, there being no mipmaps).
pub fn ground_textured(
    hx: f32,
    y: f32,
    z_far: f32,
    z_near: f32,
    texels_per_unit: f32,
    near_color: [u8; 4],
    far_color: [u8; 4],
) -> (Vec<TexVertex>, Vec<u16>) {
    let v = [
        (-hx, z_near, near_color),
        (hx, z_near, near_color),
        (hx, z_far, far_color),
        (-hx, z_far, far_color),
    ]
    .iter()
    .map(|&(x, z, color)| TexVertex {
        pos: [x, y, z],
        normal: [0.0, 1.0, 0.0],
        uv: [x * texels_per_unit, z * texels_per_unit],
        color,
    })
    .collect();
    (v, alloc::vec![0, 1, 2, 0, 2, 3])
}

/// Large flat quad facing up at height `y`, from (-hx, -hz_far) to (hx, hz_near).
pub fn ground(
    hx: f32,
    y: f32,
    z_far: f32,
    z_near: f32,
    color: [u8; 4],
) -> (Vec<LitVertex>, Vec<u16>) {
    let v = [(-hx, z_near), (hx, z_near), (hx, z_far), (-hx, z_far)]
        .iter()
        .map(|&(x, z)| LitVertex {
            pos: [x, y, z],
            normal: [0.0, 1.0, 0.0],
            color,
        })
        .collect();
    (v, alloc::vec![0, 1, 2, 0, 2, 3])
}
