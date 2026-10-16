//! nxgfx: what a game needs from the console. `Graphics::open` takes the
//! screen from the UI, powers the GPU and sets up the images to draw on;
//! a `Frame` records a command stream that `present` runs on the GPU and
//! then shows at the next vertical blank; `poll` returns the input the
//! console routes to the game (touch, gamepad).
//!
//! The panel is portrait: the landscape picture (1280 x 720, the one the
//! UI shows) is the image turned a quarter. The projection does the
//! turn, at no cost, through `Graphics::panel`.

#![no_std]

extern crate alloc;

pub mod math;
pub mod os;

use alloc::vec::Vec;

use nxk_abi::proto::{display, gpu, input};
use nxk_abi::shm;
use nxk_abi::user::Msg;

pub use math::{vec3, Mat4, Vec3};
pub use os::{now_ns, sleep_ns, Result};

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
    words: Vec<u32>,
    /// Frames presented.
    pub frames: u64,
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
        Ok(Self {
            display,
            gpu: gpu_s,
            images,
            shown: 0,
            depth,
            cmd,
            cmd_id: word(&c, 0),
            words: Vec::with_capacity(4096),
            frames: 0,
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

    fn buffer(&self, data: &[u8]) -> Result<(Shm, u64)> {
        let shm = Shm::new(data.len().max(4), shm::UNCACHED)?;
        shm.write(0, data);
        let m = Msg::from_words(gpu::BUFFER_ATTACH, &[shm.len() as u64]);
        let id = word(&self.gpu.call_with(m, shm.handle())?, 0);
        Ok((shm, id))
    }

    pub fn mesh_lit(&self, vertices: &[LitVertex], indices: &[u16]) -> Result<Mesh> {
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

    pub fn mesh_flat(&self, vertices: &[FlatVertex], indices: &[u16]) -> Result<Mesh> {
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

    /// A new frame, drawing on the image that is not on display.
    pub fn frame(&mut self) -> Frame<'_> {
        self.words.clear();
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

    /// Color and depth to `rgba` and 1.0.
    pub fn clear(&mut self, rgba: [f32; 4]) {
        self.push(gpu::CMD_CLEAR, &rgba.map(f32::to_bits));
    }

    pub fn pipeline(&mut self, pipe: Pipe, cull_back: bool) {
        let p = match pipe {
            Pipe::Flat => gpu::PIPE_FLAT,
            Pipe::Lit => gpu::PIPE_LIT,
        };
        self.push(
            gpu::CMD_PIPELINE,
            &[p, if cull_back { gpu::PF_CULL_BACK } else { 0 }],
        );
    }

    /// Constant buffer data for the draws that follow with `Pipe::Lit`.
    pub fn lit(&mut self, u: &LitUniforms) {
        self.push(gpu::CMD_UNIFORMS, &u.words());
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
