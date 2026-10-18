//! The 3D engine (MAXWELL_B, class 0xB197): state, shaders with their
//! program headers, depth, constant buffers, clears and draws into a
//! linear render target. Method numbers from NVIDIA's open-gpu-doc
//! (classes/3d/cl9097.h, clb197.h), sequences from Mesa's Gallium nvc0
//! (nvc0_screen.c, nvc0_state_validate.c, nvc0_shader_state.c) and NVK;
//! shader encodings and the program header (SPH version 3,
//! classes/3d/cla097sph.h) as NAK writes them.

extern crate alloc;

use alloc::vec::Vec;

use libnx::abi::proto::gpu as proto;

use crate::arena::Arena;
use crate::engine::Surface;
use crate::hw::{Dma, Result};
use crate::mmu::Vm;
use crate::push::{Push, SET_OBJECT, SUBC_3D};
use crate::sass::{
    ald, ast, exit, fadd_c, ffma_cb, fmul, fmul_c, ipa, mov, mov32i, mov_cb, mufu_rcp, tex_b, Asm,
    IpaMode, Sched, RZ,
};

pub const MAXWELL_B: u32 = 0xB197;

// Methods (cl9097.h).
const INVALIDATE_SHADER_CACHES: u32 = 0x021C;
const SET_RASTER_ENABLE: u32 = 0x037C;
const SET_SHADER_LOCAL_MEMORY_WINDOW: u32 = 0x077C;
const SET_COLOR_TARGET_A: u32 = 0x0800;
const SET_VIEWPORT_SCALE_X: u32 = 0x0A00;
const SET_VIEWPORT_CLIP_HORIZONTAL: u32 = 0x0C00;
const SET_VERTEX_ARRAY_START: u32 = 0x0D74;
const DRAW_VERTEX_ARRAY: u32 = 0x0D78;
const SET_COLOR_CLEAR_VALUE: u32 = 0x0D80;
const SET_Z_CLEAR_VALUE: u32 = 0x0D90;
const SET_SCISSOR_ENABLE: u32 = 0x0E00;
const SET_ZT_A: u32 = 0x0FE0;
const SET_SURFACE_CLIP_HORIZONTAL: u32 = 0x0FF4;
const SET_CLEAR_SURFACE_CONTROL: u32 = 0x10F8;
const SET_VERTEX_ATTRIBUTE_A: u32 = 0x1160;
const SET_CT_SELECT: u32 = 0x121C;
const SET_ZT_SIZE_A: u32 = 0x1228;
const SET_DEPTH_TEST: u32 = 0x12CC;
const SET_DEPTH_WRITE: u32 = 0x12E8;
const SET_DEPTH_FUNC: u32 = 0x130C;
const SET_SAMPLER_BINDING: u32 = 0x1234;
const INVALIDATE_SAMPLER_CACHE: u32 = 0x1330;
const INVALIDATE_TEXTURE_HEADER_CACHE: u32 = 0x1334;
const INVALIDATE_TEXTURE_DATA_CACHE: u32 = 0x1338;
const SET_VERTEX_STREAM_INSTANCE_A: u32 = 0x1880;
const SET_BLEND: u32 = 0x1360;
const SET_BLEND_SEPARATE_FOR_ALPHA: u32 = 0x133C;
const SET_BLEND_COLOR_OP: u32 = 0x1340;
const SET_WINDOW_ORIGIN: u32 = 0x13AC;
const SET_ZT_SELECT: u32 = 0x1538;
const SET_TEX_SAMPLER_POOL_A: u32 = 0x155C;
const SET_TEX_HEADER_POOL_A: u32 = 0x1574;
const SET_PROGRAM_REGION_A: u32 = 0x1608;
const END: u32 = 0x1614;
const BEGIN: u32 = 0x1618;
const CHECK_SPH_VERSION: u32 = 0x16A8;
const SET_INDEX_BUFFER_A: u32 = 0x17C8;
const DRAW_INDEX_BUFFER: u32 = 0x17E0;
const OGL_SET_CULL: u32 = 0x1918;
const OGL_SET_FRONT_FACE: u32 = 0x191C;
const OGL_SET_CULL_FACE: u32 = 0x1920;
const SET_VIEWPORT_SCALE_OFFSET: u32 = 0x192C;
const CLEAR_SURFACE: u32 = 0x19D0;
const SET_CT_WRITE: u32 = 0x1A00;
const SET_VERTEX_STREAM_A_FORMAT: u32 = 0x1C00;
const SET_VERTEX_STREAM_LIMIT_A_A: u32 = 0x1F00;
const SET_PIPELINE_SHADER: u32 = 0x2000;
const PIPELINE_STRIDE: u32 = 0x40;
const SET_CONSTANT_BUFFER_SELECTOR_A: u32 = 0x2380;
const BIND_GROUP_CONSTANT_BUFFER: u32 = 0x2410;

/// `SET_PIPELINE_SHADER` index = type: vertex, pixel.
const TYPE_VERTEX: u32 = 1;
const TYPE_PIXEL: u32 = 5;
/// Constant buffer binding groups: vertex, fragment.
const GROUP_VERTEX: u32 = 0;
const GROUP_PIXEL: u32 = 4;
const PRIM_TRIANGLES: u32 = 4;
/// `BEGIN`'s instance field: the next instance of the draw (1 << 26).
const INSTANCE_NEXT: u32 = 1 << 26;
/// Linear (pitch) layout in `SET_COLOR_TARGET_MEMORY`.
const LAYOUT_PITCH: u32 = 1 << 12;
const ZT_FORMAT_ZF32: u32 = 0x0A;
const DEPTH_LESS: u32 = 0x201;
/// ZT blocks are 16 GOBs high (`SET_ZT_BLOCK_SIZE` height 4).
const ZT_BLOCK_HEIGHT_LOG2: u32 = 4;
const GOB_BYTES: u64 = 512;
const GOB_WIDTH: u64 = 64;
const GOB_ROWS: u64 = 8;

/// Constant buffer slot the shaders read their parameters from.
pub const UBO_SLOT: u8 = 1;
/// Parameters of `Pipe::Lit` (words, in constant buffer 1): model-view-
/// projection rows 0-15, normal matrix rows 16-27 (three vec4), light
/// direction 28-30 and ambient 31, diffuse strength 32.
pub const LIT_UBO_WORDS: usize = 33;
/// `Pipe::Textured` adds the texture handle (header index | sampler index
/// << 20), which the service fills in, as word 33.
pub const TEX_UBO_WORDS: usize = LIT_UBO_WORDS + 1;
const U_MVP: u32 = 0x00;
const U_NORMAL: u32 = 0x40;
const U_LIGHT: u32 = 0x70;
const U_DIFFUSE: u32 = 0x80;
const U_TEXTURE: u32 = 0x84;

/// Vertex shader outputs and fragment shader inputs.
const A_POSITION: u32 = 0x70;
const A_FRAG_W: u32 = 0x7C;
const A_GENERIC0: u32 = 0x80;
const A_GENERIC1: u32 = 0x90;
const A_GENERIC2: u32 = 0xA0;
const A_GENERIC3: u32 = 0xB0;

/// Program header, then code.
const SPH_WORDS: usize = 20;
const SPH_BYTES: usize = SPH_WORDS * 4;
/// Headers at 0x30 modulo 0x80, so that the code that follows (0x50
/// later) starts on a 0x80 boundary, as Kepler and later want.
const FLAT_VS: usize = 0x030;
const FLAT_FS: usize = 0x430;
const LIT_VS: usize = 0x830;
const LIT_FS: usize = 0x1030;
const TEX_VS: usize = 0x2030;
const TEX_FS: usize = 0x2830;
const REGION_SIZE: usize = 0x3000;
/// Room past the built-in shaders for the client's.
const USER_HEAP: usize = 0x40000;
/// Constant buffer data of a batch.
const UBO_SIZE: usize = 0x40000;
/// Textures a client may have attached; entries in the header and
/// sampler pools (32 bytes each).
pub const MAX_TEXTURES: usize = 64;
const POOL_ENTRY: usize = 32;

/// Which shaders, and the vertex layout they read.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Pipe {
    /// Clip-space position and color, four floats each (32 bytes).
    Flat,
    /// Position and normal (three floats each), color (four unorm8):
    /// 28 bytes; lit by one directional light (`LIT_UBO_WORDS`).
    Lit,
    /// `Lit` and texture coordinates (two floats after the normal): 36
    /// bytes; the color is multiplied by the texel of the bound texture.
    Textured,
}

impl Pipe {
    pub fn stride(self) -> u32 {
        match self {
            Pipe::Flat => 32,
            Pipe::Lit => 28,
            Pipe::Textured => 36,
        }
    }

    fn shaders(self) -> [(u32, usize, u32, u32); 2] {
        // (type, header, registers, binding group)
        match self {
            Pipe::Flat => [
                (TYPE_VERTEX, FLAT_VS, 8, GROUP_VERTEX),
                (TYPE_PIXEL, FLAT_FS, 8, GROUP_PIXEL),
            ],
            Pipe::Lit => [
                (TYPE_VERTEX, LIT_VS, 24, GROUP_VERTEX),
                (TYPE_PIXEL, LIT_FS, 16, GROUP_PIXEL),
            ],
            Pipe::Textured => [
                (TYPE_VERTEX, TEX_VS, 24, GROUP_VERTEX),
                (TYPE_PIXEL, TEX_FS, 32, GROUP_PIXEL),
            ],
        }
    }

    /// `SET_VERTEX_ATTRIBUTE_A` words, one per attribute: stream 0,
    /// offset, component widths, numeric type.
    fn attributes(self) -> Vec<u32> {
        const R32X4: u32 = 1;
        const R32X3: u32 = 2;
        const R32X2: u32 = 4;
        const R8X4: u32 = 0x0A;
        const UNORM: u32 = 2;
        const FLOAT: u32 = 7;
        let a = |offset: u32, width: u32, ty: u32| width << 21 | ty << 27 | offset << 7;
        match self {
            Pipe::Flat => alloc::vec![a(0, R32X4, FLOAT), a(16, R32X4, FLOAT)],
            Pipe::Lit => {
                alloc::vec![a(0, R32X3, FLOAT), a(12, R32X3, FLOAT), a(24, R8X4, UNORM)]
            }
            Pipe::Textured => alloc::vec![
                a(0, R32X3, FLOAT),
                a(12, R32X3, FLOAT),
                a(24, R32X2, FLOAT),
                a(32, R8X4, UNORM)
            ],
        }
    }
}

/// How vertices sit in a vertex buffer: bytes each, and the
/// `SET_VERTEX_ATTRIBUTE_A` words (`attr_word`).
pub struct Layout<'a> {
    pub stride: u32,
    pub attrs: &'a [u32],
    /// Bytes per instance in stream 1 (0: none; attributes of `attrs`
    /// that read it are marked by `attr_stream`).
    pub inst_stride: u32,
}

/// Where the instances' data is, and how many of them.
#[derive(Clone, Copy)]
pub struct Instances {
    pub va: u64,
    pub count: u32,
}

/// Stream of the per-instance data.
pub const INSTANCE_STREAM: u32 = 1;

/// A client pipeline: its shaders (program header offsets), their
/// registers, and the draw state.
#[derive(Clone)]
pub struct Custom {
    pub vs: u32,
    pub vs_gprs: u32,
    pub fs: u32,
    pub fs_gprs: u32,
    pub stride: u32,
    /// One word per attribute slot, 16 of them.
    pub attrs: Vec<u32>,
    /// Bytes per instance (0: no attribute reads the instance stream).
    pub inst_stride: u32,
    pub flags: u32,
    pub blend: u32,
    pub depth: u32,
}

/// A vertex attribute word: `proto::AF_*` format at a byte offset.
pub fn attr_word(offset: u32, format: u32) -> Option<u32> {
    const FLOAT: u32 = 7;
    const UNORM: u32 = 2;
    let (width, ty) = match format {
        proto::AF_F32X1 => (0x12, FLOAT),
        proto::AF_F32X2 => (4, FLOAT),
        proto::AF_F32X3 => (2, FLOAT),
        proto::AF_F32X4 => (1, FLOAT),
        proto::AF_U8X4_UNORM => (0x0A, UNORM),
        _ => return None,
    };
    (offset < 1 << 14).then_some(width << 21 | ty << 27 | offset << 7)
}

/// Program header offsets of the built-in flat shaders and their registers.
pub const FLAT_VS_AT: u32 = FLAT_VS as u32;
pub const FLAT_FS_AT: u32 = FLAT_FS as u32;
pub const FLAT_GPRS: u32 = 8;

/// A vertex attribute slot that feeds nothing: inactive, but with a valid
/// format (R32_G32_B32_A32 float, as NVK writes it), or the front end
/// refuses the value (data error at SET_VERTEX_ATTRIBUTE_A).
pub const ATTR_INACTIVE: u32 = 1 << 6 | 1 << 21 | 7 << 27;

/// Depth buffer: 32-bit float, block-linear (the engine does not take it
/// pitch-linear), `bytes` of memory for a `width` x `height` image.
#[derive(Clone, Copy)]
pub struct Depth {
    pub va: u64,
    pub width: u32,
    pub height: u32,
}

impl Depth {
    pub fn bytes(width: u32, height: u32) -> usize {
        let gobs_x = (width as u64 * 4).div_ceil(GOB_WIDTH);
        let block_rows = GOB_ROWS << ZT_BLOCK_HEIGHT_LOG2;
        let blocks_y = (height as u64).div_ceil(block_rows);
        (gobs_x * blocks_y * (GOB_BYTES << ZT_BLOCK_HEIGHT_LOG2)) as usize
    }
}

/// A block-linear color surface: GOBs of 64 x 8 bytes, stacked
/// `1 << block_log2` high in a block. The engine takes a depth buffer
/// only with a surface like this one (Gallium does the same).
#[derive(Clone, Copy)]
pub struct Tiled {
    pub va: u64,
    pub width: u32,
    pub height: u32,
    /// `SET_COLOR_TARGET_FORMAT` value (`color_format`).
    pub format: u32,
    /// Bytes per pixel.
    pub bpp: u32,
    pub block_log2: u32,
}

impl Tiled {
    pub fn bytes(&self) -> usize {
        proto::tiled_bytes_bpp(self.width, self.height, self.bpp, self.block_log2)
    }

    /// Byte offset of pixel (x, y).
    pub fn offset(&self, x: u32, y: u32) -> usize {
        proto::tiled_offset(self.width, x, y, self.bpp, self.block_log2)
    }
}

/// `SET_COLOR_TARGET_FORMAT` value of a `proto::TF_*` format.
pub fn color_format(tf: u64) -> Option<u32> {
    match tf {
        proto::TF_RGBA8 => Some(proto::FMT_A8B8G8R8 as u32),
        proto::TF_R32F => Some(0xE5),
        proto::TF_RGBA16F => Some(0xCA),
        proto::TF_RGBA32F => Some(0xC0),
        _ => None,
    }
}

/// The color part of a target: pitch-linear or block-linear.
#[derive(Clone, Copy)]
pub enum Color {
    Linear(Surface),
    Tiled(Tiled),
}

impl Color {
    fn size(&self) -> (u32, u32) {
        match self {
            Color::Linear(s) => (s.width, s.height),
            Color::Tiled(t) => (t.width, t.height),
        }
    }
}

/// Where a batch draws.
#[derive(Clone, Copy)]
pub struct Target {
    pub color: Color,
    pub depth: Option<Depth>,
}

fn field(w: &mut [u32; SPH_WORDS], lo: usize, v: u32) {
    w[lo / 32] |= v << (lo % 32);
}

/// Vertex shader header (NAK `encode_header`): `attrs_in` is the mask of
/// input words (4 per attribute), `attrs_out` that of generic output
/// words, plus the position.
fn vs_header(attrs_in: u32, attrs_out: u32) -> [u32; SPH_WORDS] {
    let mut h = [0u32; SPH_WORDS];
    field(&mut h, 0, 1); // SPH type: VTG
    field(&mut h, 5, 3); // version
    field(&mut h, 10, 1); // vertex shader
    field(&mut h, 17, 1); // SASS version
    field(&mut h, 140, 0xFF); // no store-request range
    field(&mut h, 192, attrs_in);
    field(&mut h, 428, 0xF); // position out
    field(&mut h, 432, attrs_out);
    h
}

/// Fragment shader header: generic input words with their interpolation
/// (`imap`: 2 bits each: 1 constant, 2 perspective, 3 screen-linear);
/// target 0 out.
fn fs_header(imap: u32) -> [u32; SPH_WORDS] {
    let mut h = [0u32; SPH_WORDS];
    field(&mut h, 0, 2); // SPH type: PS
    field(&mut h, 5, 3);
    field(&mut h, 10, 5); // pixel shader
    field(&mut h, 14, 1); // MRT enable (color 0 broadcast)
    field(&mut h, 17, 1);
    field(&mut h, 191, 1); // reads position.w, as Gallium declares it
    field(&mut h, 192, imap);
    field(&mut h, 576, 0xF); // target 0 out: RGBA
    h
}

/// Fixed-latency result read by the next instruction.
fn alu() -> Sched {
    Sched::stall(6)
}

/// Sets barrier `sb`, waited on by a later instruction: a barrier arms a
/// cycle after issue, so with a stall of 1 the very next wait would pass
/// on a stale register (the same rule as `kernels.rs`).
fn sets(sb: u8) -> Sched {
    Sched::stall(2).wr(sb)
}

fn flat_vs() -> Vec<u64> {
    let mut a = Asm::new();
    // Input attribute n is at 0x80 + 16 n, as outputs are.
    a.emit(ald(0, A_GENERIC0, 4), sets(0));
    a.emit(ald(4, A_GENERIC1, 4), sets(1));
    a.emit(ast(0, A_POSITION, 4), Sched::stall(1).rd(2).wait(1));
    a.emit(ast(4, A_GENERIC0, 4), Sched::stall(1).rd(3).wait(2));
    a.emit(exit(), Sched::stall(5).wait(0x3F));
    a.finish()
}

fn flat_fs() -> Vec<u64> {
    let mut a = Asm::new();
    for c in 0..4u8 {
        a.emit(
            ipa(c, A_GENERIC0 + 4 * c as u32, IpaMode::Pass, RZ),
            sets(0),
        );
    }
    a.emit(exit(), Sched::stall(5).wait(0x3F));
    a.finish()
}

/// clip = MVP * (position, 1); normal = N * normal; color through.
fn lit_vs() -> Vec<u64> {
    let mut a = Asm::new();
    a.emit(ald(0, A_GENERIC0, 3), sets(0)); // R0-R2 position
    a.emit(ald(4, A_GENERIC1, 3), sets(1)); // R4-R6 normal
    a.emit(ald(8, A_GENERIC2, 4), sets(2)); // R8-R11 color
                                            // One matrix row: d = m.x * s + m.y * (s + 1) + m.z * (s + 2) (+ m.w);
                                            // the first multiply waits for the attribute `barrier`.
    let row = |a: &mut Asm, d: u8, s: u8, at: u32, w: bool, barrier: u8| {
        a.emit(
            fmul_c(d, s, UBO_SLOT, at),
            Sched::stall(6).wait(1 << barrier),
        );
        a.emit(ffma_cb(d, s + 1, UBO_SLOT, at + 4, d), alu());
        a.emit(ffma_cb(d, s + 2, UBO_SLOT, at + 8, d), alu());
        if w {
            a.emit(fadd_c(d, d, UBO_SLOT, at + 12), alu());
        }
    };
    for r in 0..4u8 {
        row(&mut a, 12 + r, 0, U_MVP + 0x10 * r as u32, true, 0);
    }
    for r in 0..3u8 {
        row(&mut a, 16 + r, 4, U_NORMAL + 0x10 * r as u32, false, 1);
    }
    a.emit(ast(12, A_POSITION, 4), Sched::stall(1).rd(3));
    a.emit(ast(16, A_GENERIC0, 3), Sched::stall(1).rd(4));
    a.emit(ast(8, A_GENERIC1, 4), Sched::stall(1).rd(5).wait(1 << 2));
    a.emit(exit(), Sched::stall(5).wait(0x3F));
    a.finish()
}

/// color = vertex color * (ambient + diffuse * saturate(N . L)).
fn lit_fs() -> Vec<u64> {
    let mut a = Asm::new();
    // 1/w, then the perspective-correct normal (R4-R6) and color (R8-R11).
    a.emit(ipa(0, A_FRAG_W, IpaMode::Pass, RZ), sets(0));
    // The attributes are interpolated divided by w: w itself is the
    // reciprocal of the interpolated 1/w.
    a.emit(mufu_rcp(0, 0), sets(0).wait(1 << 0));
    for (k, (d, addr)) in [
        (4u8, A_GENERIC0),
        (5, A_GENERIC0 + 4),
        (6, A_GENERIC0 + 8),
        (8, A_GENERIC1),
        (9, A_GENERIC1 + 4),
        (10, A_GENERIC1 + 8),
        (11, A_GENERIC1 + 12),
    ]
    .into_iter()
    .enumerate()
    {
        let s = sets(1);
        a.emit(
            ipa(d, addr, IpaMode::PassMulW, 0),
            if k == 0 { s.wait(1 << 0) } else { s },
        );
    }
    // N . L
    a.emit(
        fmul_c(12, 4, UBO_SLOT, U_LIGHT),
        Sched::stall(6).wait(1 << 1),
    );
    a.emit(ffma_cb(12, 5, UBO_SLOT, U_LIGHT + 4, 12), alu());
    a.emit(ffma_cb(12, 6, UBO_SLOT, U_LIGHT + 8, 12), alu());
    // saturate: times 1.0, clamped.
    a.emit(mov32i(13, 1.0f32.to_bits()), alu());
    a.emit(fmul(12, 12, 13, true), alu());
    // ambient + diffuse * saturate(N . L)
    a.emit(mov_cb(14, UBO_SLOT, U_LIGHT + 12), alu());
    a.emit(ffma_cb(12, 12, UBO_SLOT, U_DIFFUSE, 14), alu());
    a.emit(fmul(0, 8, 12, false), alu());
    a.emit(fmul(1, 9, 12, false), alu());
    a.emit(fmul(2, 10, 12, false), alu());
    a.emit(mov(3, 11), alu());
    a.emit(exit(), Sched::stall(5).wait(0x3F));
    a.finish()
}

/// As `lit_vs`, with texture coordinates (attribute 2, two words) passed
/// through to generic output 2.
fn tex_vs() -> Vec<u64> {
    let mut a = Asm::new();
    a.emit(ald(0, A_GENERIC0, 3), sets(0)); // R0-R2 position
    a.emit(ald(4, A_GENERIC1, 3), sets(1)); // R4-R6 normal
    a.emit(ald(8, A_GENERIC2, 2), sets(2)); // R8-R9 texture coordinates
    a.emit(ald(20, A_GENERIC3, 4), sets(3)); // R20-R23 color
    let row = |a: &mut Asm, d: u8, s: u8, at: u32, w: bool, barrier: u8| {
        a.emit(
            fmul_c(d, s, UBO_SLOT, at),
            Sched::stall(6).wait(1 << barrier),
        );
        a.emit(ffma_cb(d, s + 1, UBO_SLOT, at + 4, d), alu());
        a.emit(ffma_cb(d, s + 2, UBO_SLOT, at + 8, d), alu());
        if w {
            a.emit(fadd_c(d, d, UBO_SLOT, at + 12), alu());
        }
    };
    for r in 0..4u8 {
        row(&mut a, 12 + r, 0, U_MVP + 0x10 * r as u32, true, 0);
    }
    for r in 0..3u8 {
        row(&mut a, 16 + r, 4, U_NORMAL + 0x10 * r as u32, false, 1);
    }
    a.emit(ast(12, A_POSITION, 4), Sched::stall(1).rd(4));
    a.emit(ast(16, A_GENERIC0, 3), Sched::stall(1).rd(5));
    a.emit(ast(20, A_GENERIC1, 4), Sched::stall(1).rd(4).wait(1 << 3));
    a.emit(ast(8, A_GENERIC2, 2), Sched::stall(1).rd(5).wait(1 << 2));
    a.emit(exit(), Sched::stall(5).wait(0x3F));
    a.finish()
}

/// color = vertex color * texel * (ambient + diffuse * saturate(N . L)).
fn tex_fs() -> Vec<u64> {
    let mut a = Asm::new();
    a.emit(ipa(0, A_FRAG_W, IpaMode::Pass, RZ), sets(0));
    a.emit(mufu_rcp(0, 0), sets(0).wait(1 << 0));
    // Normal R4-R6, color R8-R11, texture coordinates R16-R17: TEX takes
    // the coordinates, and the handle in a vector of its own (R20).
    for (k, (d, addr)) in [
        (4u8, A_GENERIC0),
        (5, A_GENERIC0 + 4),
        (6, A_GENERIC0 + 8),
        (8, A_GENERIC1),
        (9, A_GENERIC1 + 4),
        (10, A_GENERIC1 + 8),
        (11, A_GENERIC1 + 12),
        (16, A_GENERIC2),
        (17, A_GENERIC2 + 4),
    ]
    .into_iter()
    .enumerate()
    {
        let s = sets(1);
        a.emit(
            ipa(d, addr, IpaMode::PassMulW, 0),
            if k == 0 { s.wait(1 << 0) } else { s },
        );
    }
    a.emit(mov_cb(20, UBO_SLOT, U_TEXTURE), alu().wait(1 << 1));
    // The texel lands in R24-R27 (barrier 2) while the light is worked out.
    a.emit(tex_b(24, 16, 20), sets(2).wait(1 << 1));
    a.emit(
        fmul_c(12, 4, UBO_SLOT, U_LIGHT),
        Sched::stall(6).wait(1 << 1),
    );
    a.emit(ffma_cb(12, 5, UBO_SLOT, U_LIGHT + 4, 12), alu());
    a.emit(ffma_cb(12, 6, UBO_SLOT, U_LIGHT + 8, 12), alu());
    a.emit(mov32i(13, 1.0f32.to_bits()), alu());
    a.emit(fmul(12, 12, 13, true), alu());
    a.emit(mov_cb(14, UBO_SLOT, U_LIGHT + 12), alu());
    a.emit(ffma_cb(12, 12, UBO_SLOT, U_DIFFUSE, 14), alu());
    // Vertex color times light, then times the texel.
    a.emit(fmul(8, 8, 12, false), alu());
    a.emit(fmul(9, 9, 12, false), alu());
    a.emit(fmul(10, 10, 12, false), alu());
    a.emit(fmul(0, 8, 24, false), alu().wait(1 << 2));
    a.emit(fmul(1, 9, 25, false), alu());
    a.emit(fmul(2, 10, 26, false), alu());
    a.emit(fmul(3, 11, 27, false), alu());
    a.emit(exit(), Sched::stall(5).wait(0x3F));
    a.finish()
}

fn bits(w: &mut [u32; 8], lo: usize, hi: usize, v: u32) {
    let n = hi - lo + 1;
    assert!(n == 32 || v < 1 << n);
    for i in 0..n {
        w[(lo + i) / 32] |= ((v >> i) & 1) << ((lo + i) % 32);
    }
}

/// An image to sample: block-linear at `va` (512-byte aligned), `format`
/// a `proto::TF_*`, blocks `1 << block_log2` GOBs high.
#[derive(Clone, Copy)]
pub struct TexImage {
    pub va: u64,
    pub width: u32,
    pub height: u32,
    pub format: u64,
    pub block_log2: u32,
    /// Mip levels, 1..=15.
    pub levels: u32,
}

/// Texture header (TIC, `TEXHEAD_BL`, as NIL's `nvb097_fill_image_view_desc`
/// writes it): block-linear, 2D, normalized.
fn tic(t: &TexImage) -> [u32; 8] {
    const SIZES_RGBA8: u32 = 8;
    const SIZES_R32: u32 = 0x0F;
    const SIZES_RGBA16: u32 = 3;
    const SIZES_RGBA32: u32 = 1;
    const UNORM: u32 = 2;
    const FLOAT: u32 = 7;
    const SOURCE_ZERO: u32 = 0;
    const SOURCE_R: u32 = 2;
    const SOURCE_ONE_FLOAT: u32 = 7;
    const BLOCKLINEAR: u32 = 3;
    const TWO_D: u32 = 1;
    const BORDER_FROM_SAMPLER: u32 = 7;
    let (sizes, ty, src) = match t.format {
        proto::TF_R32F => (
            SIZES_R32,
            FLOAT,
            [SOURCE_R, SOURCE_ZERO, SOURCE_ZERO, SOURCE_ONE_FLOAT],
        ),
        proto::TF_RGBA16F => (SIZES_RGBA16, FLOAT, [SOURCE_R, 3, 4, 5]),
        proto::TF_RGBA32F => (SIZES_RGBA32, FLOAT, [SOURCE_R, 3, 4, 5]),
        _ => (SIZES_RGBA8, UNORM, [SOURCE_R, 3, 4, 5]),
    };
    assert!(t.va.is_multiple_of(512) && t.va >> 48 == 0);
    let mut w = [0u32; 8];
    bits(&mut w, 0, 6, sizes);
    for c in 0..4 {
        bits(&mut w, 7 + 3 * c, 9 + 3 * c, ty);
        bits(&mut w, 19 + 3 * c, 21 + 3 * c, src[c]);
    }
    bits(&mut w, 41, 63, (t.va >> 9) as u32 & 0x7F_FFFF);
    bits(&mut w, 64, 79, (t.va >> 32) as u32);
    bits(&mut w, 85, 87, BLOCKLINEAR);
    // One GOB wide blocks.
    bits(&mut w, 99, 101, t.block_log2);
    // LOD quality as NIL sets it.
    bits(&mut w, 112, 114, 0b111);
    // Last level of the chain, and of the view (from level 0).
    bits(&mut w, 124, 127, t.levels - 1);
    bits(&mut w, 228, 231, t.levels - 1);
    bits(&mut w, 128, 143, t.width - 1);
    bits(&mut w, 151, 154, TWO_D);
    bits(&mut w, 157, 159, BORDER_FROM_SAMPLER);
    bits(&mut w, 160, 175, t.height - 1);
    bits(&mut w, 191, 191, 1); // normalized coordinates
    bits(&mut w, 215, 216, 2); // aniso spread functions, as NIL
    bits(&mut w, 217, 218, 1);
    w
}

/// Sampler (TSC): filter and wrap mode, a blend of two levels (the
/// nearest with `TEX_NEAREST`) if the texture has several, transparent border.
fn tsc(flags: u64, levels: u32) -> [u32; 8] {
    const WRAP: u32 = 0;
    const CLAMP_TO_EDGE: u32 = 2;
    const POINT: u32 = 1;
    const LINEAR: u32 = 2;
    const MIP_NONE: u32 = 1;
    const MIP_POINT: u32 = 2;
    const MIP_LINEAR: u32 = 3;
    const AUTO_SPAN_SEAM: u32 = 2;
    let (filter, wrap) = (
        if flags & proto::TEX_NEAREST != 0 {
            POINT
        } else {
            LINEAR
        },
        if flags & proto::TEX_CLAMP != 0 {
            CLAMP_TO_EDGE
        } else {
            WRAP
        },
    );
    let mut w = [0u32; 8];
    bits(&mut w, 0, 2, wrap);
    bits(&mut w, 3, 5, wrap);
    bits(&mut w, 6, 8, wrap);
    bits(&mut w, 14, 16, 1); // font filter width and height: 2
    bits(&mut w, 17, 19, 1);
    bits(&mut w, 32, 34, filter); // magnification
    bits(&mut w, 36, 37, filter); // minification
    let mip = match (levels > 1, flags & proto::TEX_NEAREST != 0) {
        (false, _) => MIP_NONE,
        (true, true) => MIP_POINT,
        (true, false) => MIP_LINEAR,
    };
    bits(&mut w, 38, 39, mip);
    bits(&mut w, 40, 41, AUTO_SPAN_SEAM);
    bits(&mut w, 76, 87, 0xFFF); // LOD clamp up to the last level
    w
}

/// Shaders in GPU memory, and the constant buffer data of a batch.
pub struct Gfx {
    prog: Dma,
    prog_va: u64,
    arena: Arena,
    ubo: Dma,
    ubo_va: u64,
    ubo_pos: usize,
    /// Texture header and sampler pools, written by the CPU.
    tics: Dma,
    tscs: Dma,
    tics_va: u64,
    tscs_va: u64,
}

impl Gfx {
    pub fn new(vm: &mut Vm) -> Result<Self> {
        let prog = Dma::new(REGION_SIZE + USER_HEAP)?;
        let lit_in = 0x7 | 0x7 << 4 | 0xF << 8;
        // Output words: normal in generic 0 (3), color in generic 1 (4).
        let lit_out = 0x7 | 0xF << 4;
        // Textured: the same, and texture coordinates in generic 2.
        let tex_in = 0x7 | 0x7 << 4 | 0x3 << 8 | 0xF << 12;
        let tex_out = lit_out | 0x3 << 8;
        // Input interpolation: perspective (2) on normal words 0-2 and
        // color words 4-7 (generic 1 at word 4: attributes are 4 words).
        let persp = |words: u32| (0..16).fold(0u32, |m, i| m | (((words >> i) & 1) * 2) << (2 * i));
        for (at, hdr, code) in [
            (FLAT_VS, vs_header(0xFF, 0xF), flat_vs()),
            (FLAT_FS, fs_header(0xFF), flat_fs()),
            (LIT_VS, vs_header(lit_in, lit_out), lit_vs()),
            (LIT_FS, fs_header(persp(0x7 | 0xF << 4)), lit_fs()),
            (TEX_VS, vs_header(tex_in, tex_out), tex_vs()),
            (TEX_FS, fs_header(persp(tex_out)), tex_fs()),
        ] {
            prog.words(at, &hdr);
            for (i, w) in code.iter().enumerate() {
                prog.w64(at + SPH_BYTES + i * 8, *w);
            }
        }
        let prog_va = vm.map(&prog, false)?;
        let ubo = Dma::new(UBO_SIZE)?;
        let ubo_va = vm.map(&ubo, false)?;
        let tics = Dma::new(MAX_TEXTURES * POOL_ENTRY)?;
        let tscs = Dma::new(MAX_TEXTURES * POOL_ENTRY)?;
        let tics_va = vm.map(&tics, false)?;
        let tscs_va = vm.map(&tscs, false)?;
        Ok(Self {
            prog,
            prog_va,
            arena: Arena::new(REGION_SIZE as u32, USER_HEAP as u32),
            ubo,
            ubo_va,
            ubo_pos: 0,
            tics,
            tscs,
            tics_va,
            tscs_va,
        })
    }

    /// Texture `id`: its header and sampler written to the pools; the
    /// image is the caller's. The batch that follows sees them (`begin`
    /// invalidates the caches).
    pub fn set_texture(&self, id: usize, img: &TexImage, flags: u64) {
        self.tics.words(id * POOL_ENTRY, &tic(img));
        self.tscs.words(id * POOL_ENTRY, &tsc(flags, img.levels));
    }

    pub fn clear_texture(&self, id: usize) {
        self.tics.words(id * POOL_ENTRY, &[0; 8]);
        self.tscs.words(id * POOL_ENTRY, &[0; 8]);
    }

    /// The handle `TEX` takes for texture `id`: header index, then the
    /// sampler of the same index.
    pub fn texture_handle(id: usize) -> u32 {
        id as u32 | (id as u32) << 20
    }

    /// Texture header pool, sampler pool (GPU addresses) and the last
    /// index of both.
    pub fn pools(&self) -> (u64, u64, u32) {
        (self.tics_va, self.tscs_va, MAX_TEXTURES as u32 - 1)
    }

    /// Channel setup: the class bound, the state that stays.
    pub fn setup(&self, p: &mut Push) {
        p.inc(SUBC_3D, SET_OBJECT, &[MAXWELL_B]);
        p.inc(SUBC_3D, SET_SHADER_LOCAL_MEMORY_WINDOW, &[0xFF << 24]);
        p.inc(
            SUBC_3D,
            SET_PROGRAM_REGION_A,
            &[(self.prog_va >> 32) as u32, self.prog_va as u32],
        );
        p.inc(SUBC_3D, CHECK_SPH_VERSION, &[3 | 3 << 16]);
        p.inc(SUBC_3D, SET_RASTER_ENABLE, &[1]);
        p.inc(SUBC_3D, SET_VIEWPORT_SCALE_OFFSET, &[1]);
        p.inc(SUBC_3D, SET_WINDOW_ORIGIN, &[0]);
        p.inc(SUBC_3D, SET_BLEND, &[0]);
        // All four channels written.
        p.inc(SUBC_3D, SET_CT_WRITE, &[0x1111]);
        // Texture pools (offset high, low, last index); a handle names the
        // sampler apart from the header.
        let last = MAX_TEXTURES as u32 - 1;
        p.inc(
            SUBC_3D,
            SET_TEX_HEADER_POOL_A,
            &[(self.tics_va >> 32) as u32, self.tics_va as u32, last],
        );
        p.inc(
            SUBC_3D,
            SET_TEX_SAMPLER_POOL_A,
            &[(self.tscs_va >> 32) as u32, self.tscs_va as u32, last],
        );
        p.inc(SUBC_3D, SET_SAMPLER_BINDING, &[0]);
    }

    /// Start of a batch: caches that may hold what the CPU rewrote, and
    /// the constant buffer data from its start.
    pub fn begin(&mut self, p: &mut Push) {
        self.ubo_pos = 0;
        p.inc(SUBC_3D, INVALIDATE_SHADER_CACHES, &[1 | 1 << 4 | 1 << 12]);
        // The pools too: lines of all three caches.
        p.inc(SUBC_3D, INVALIDATE_SAMPLER_CACHE, &[0]);
        p.inc(SUBC_3D, INVALIDATE_TEXTURE_HEADER_CACHE, &[0]);
        p.inc(SUBC_3D, INVALIDATE_TEXTURE_DATA_CACHE, &[0]);
    }

    /// The render target, its depth buffer and the viewport.
    fn target(&self, p: &mut Push, t: &Target) {
        let (width, height) = t.color.size();
        p.inc(
            SUBC_3D,
            SET_SURFACE_CLIP_HORIZONTAL,
            &[width << 16, height << 16],
        );
        let rt = match t.color {
            // Pitch layouts give the pitch as the width.
            Color::Linear(s) => [
                (s.va >> 32) as u32,
                s.va as u32,
                s.pitch,
                s.height,
                s.format,
                LAYOUT_PITCH,
                1,
                0,
                0,
            ],
            Color::Tiled(c) => [
                (c.va >> 32) as u32,
                c.va as u32,
                c.width,
                c.height,
                c.format,
                c.block_log2 << 4,
                1,
                (c.bytes() >> 2) as u32,
                0,
            ],
        };
        p.inc(SUBC_3D, SET_COLOR_TARGET_A, &rt);
        p.inc(SUBC_3D, SET_CT_SELECT, &[1]);
        match t.depth {
            Some(d) => {
                let size = Depth::bytes(d.width, d.height) as u32;
                p.inc(
                    SUBC_3D,
                    SET_ZT_A,
                    &[
                        (d.va >> 32) as u32,
                        d.va as u32,
                        ZT_FORMAT_ZF32,
                        ZT_BLOCK_HEIGHT_LOG2 << 4,
                        size >> 2,
                    ],
                );
                p.inc(SUBC_3D, SET_ZT_SIZE_A, &[d.width, d.height, 1 | 1 << 16]);
                p.inc(SUBC_3D, SET_ZT_SELECT, &[1]);
                p.inc(SUBC_3D, SET_DEPTH_TEST, &[1]);
                p.inc(SUBC_3D, SET_DEPTH_FUNC, &[DEPTH_LESS]);
                p.inc(SUBC_3D, SET_DEPTH_WRITE, &[1]);
            }
            None => {
                p.inc(SUBC_3D, SET_ZT_SELECT, &[0]);
                p.inc(SUBC_3D, SET_DEPTH_TEST, &[0]);
            }
        }
        let (w, h) = (width as f32, height as f32);
        // NDC (-1..1) to the window, y down.
        p.inc(
            SUBC_3D,
            SET_VIEWPORT_SCALE_X,
            &[
                (w / 2.0).to_bits(),
                (h / 2.0).to_bits(),
                0.5f32.to_bits(),
                (w / 2.0).to_bits(),
                (h / 2.0).to_bits(),
                0.5f32.to_bits(),
            ],
        );
        p.inc(
            SUBC_3D,
            SET_VIEWPORT_CLIP_HORIZONTAL,
            &[
                width << 16,
                height << 16,
                0.0f32.to_bits(),
                1.0f32.to_bits(),
            ],
        );
        p.inc(SUBC_3D, SET_SCISSOR_ENABLE, &[1, width << 16, height << 16]);
    }

    /// Whole target set to `rgba`; the depth buffer, if any, to 1.0.
    pub fn clear(&self, p: &mut Push, t: &Target, rgba: [f32; 4]) {
        self.target(p, t);
        p.inc(SUBC_3D, SET_COLOR_CLEAR_VALUE, &rgba.map(|c| c.to_bits()));
        p.inc(SUBC_3D, SET_Z_CLEAR_VALUE, &[1.0f32.to_bits()]);
        p.inc(SUBC_3D, SET_CLEAR_SURFACE_CONTROL, &[0]);
        let z = t.depth.is_some() as u32;
        p.inc(SUBC_3D, CLEAR_SURFACE, &[0xF << 2 | z]);
    }

    /// Select shaders and the draw state for the draws that follow.
    pub fn pipeline(&self, p: &mut Push, t: &Target, pipe: Pipe, cull_back: bool) {
        self.target(p, t);
        for (ty, at, gprs, group) in pipe.shaders() {
            let m = SET_PIPELINE_SHADER + ty * PIPELINE_STRIDE;
            // Shader (enabled, type), program offset; register count and
            // binding group after the reserved word.
            p.inc(SUBC_3D, m, &[1 | ty << 4, at as u32]);
            p.inc(SUBC_3D, m + 0xC, &[gprs, group]);
        }
        p.inc(SUBC_3D, OGL_SET_CULL, &[cull_back as u32]);
        p.inc(SUBC_3D, OGL_SET_FRONT_FACE, &[0x901]);
        p.inc(SUBC_3D, OGL_SET_CULL_FACE, &[0x405]);
        // Back to the state the built-in pipelines draw with.
        p.inc(SUBC_3D, SET_BLEND, &[0]);
        p.inc(SUBC_3D, SET_DEPTH_FUNC, &[DEPTH_LESS]);
        p.inc(SUBC_3D, SET_CT_WRITE, &[0x1111]);
    }

    /// Install a client's shader in the program region: header and code
    /// together. The offset of its header and the block to give back.
    pub fn install(&mut self, header: &[u32; 20], code: &[u64]) -> Option<(u32, u32, u32)> {
        // Headers at 0x30 modulo 0x80, code 0x50 later; room for the
        // instruction prefetch past the last instruction.
        let len = (0x80 + code.len() * 8 + 0x80).next_multiple_of(0x80) as u32;
        let block = self.arena.alloc(len, 0x80)?;
        let hdr = block + 0x30;
        self.prog.words(hdr as usize, header);
        for (i, w) in code.iter().enumerate() {
            self.prog.w64(block as usize + 0x80 + i * 8, *w);
        }
        Some((hdr, block, len))
    }

    pub fn uninstall(&mut self, block: u32, len: u32) {
        self.arena.release(block, len);
    }

    /// Room for `bytes` (256-aligned) in this batch's constant buffer
    /// memory: its offset and GPU address. The caller writes through
    /// `ubo_write`.
    pub fn alloc_ubo(&mut self, bytes: usize) -> Option<(usize, u64)> {
        let at = self.ubo_pos.next_multiple_of(256);
        if at + bytes.next_multiple_of(256) > UBO_SIZE {
            return None;
        }
        self.ubo_pos = at + bytes;
        Some((at, self.ubo_va + at as u64))
    }

    pub fn ubo_write(&self, at: usize, bytes: &[u8]) {
        self.ubo.write(at, bytes);
    }

    /// Shaders and state of a client pipeline for the draws that follow.
    pub fn pipeline_custom(&self, p: &mut Push, t: &Target, c: &Custom) {
        self.target(p, t);
        for (ty, at, gprs, group) in [
            (TYPE_VERTEX, c.vs, c.vs_gprs, GROUP_VERTEX),
            (TYPE_PIXEL, c.fs, c.fs_gprs, GROUP_PIXEL),
        ] {
            let m = SET_PIPELINE_SHADER + ty * PIPELINE_STRIDE;
            p.inc(SUBC_3D, m, &[1 | ty << 4, at]);
            p.inc(SUBC_3D, m + 0xC, &[gprs, group]);
        }
        let cull = c.flags & (proto::PF_CULL_BACK | proto::PF_CULL_FRONT) != 0;
        p.inc(SUBC_3D, OGL_SET_CULL, &[cull as u32]);
        p.inc(SUBC_3D, OGL_SET_FRONT_FACE, &[0x901]);
        let face = if c.flags & proto::PF_CULL_FRONT != 0 {
            0x404
        } else {
            0x405
        };
        p.inc(SUBC_3D, OGL_SET_CULL_FACE, &[face]);
        if t.depth.is_some() {
            let test = c.flags & proto::PF_NO_DEPTH_TEST == 0;
            p.inc(SUBC_3D, SET_DEPTH_TEST, &[test as u32]);
            p.inc(
                SUBC_3D,
                SET_DEPTH_WRITE,
                &[(c.flags & proto::PF_NO_DEPTH_WRITE == 0) as u32],
            );
            let func = match c.depth {
                proto::DEPTH_LEQUAL => 0x203,
                proto::DEPTH_GREATER => 0x204,
                proto::DEPTH_GEQUAL => 0x206,
                proto::DEPTH_EQUAL => 0x202,
                proto::DEPTH_ALWAYS => 0x207,
                _ => DEPTH_LESS,
            };
            p.inc(SUBC_3D, SET_DEPTH_FUNC, &[func]);
        }
        // Blend equations (OGL values of cl9097.h).
        const ADD: u32 = 0x8006;
        const ZERO: u32 = 0x4000;
        const ONE: u32 = 0x4001;
        const SRC_ALPHA: u32 = 0x4302;
        const INV_SRC_ALPHA: u32 = 0x4303;
        // (colour source, colour dest, alpha source, alpha dest)
        let eq = match c.blend {
            proto::BLEND_ALPHA => Some([SRC_ALPHA, INV_SRC_ALPHA, ONE, INV_SRC_ALPHA]),
            proto::BLEND_ADD => Some([SRC_ALPHA, ONE, ZERO, ONE]),
            proto::BLEND_PREMUL => Some([ONE, INV_SRC_ALPHA, ONE, INV_SRC_ALPHA]),
            _ => None,
        };
        if let Some(e) = eq {
            p.inc(SUBC_3D, SET_BLEND_SEPARATE_FOR_ALPHA, &[1]);
            p.inc(
                SUBC_3D,
                SET_BLEND_COLOR_OP,
                &[ADD, e[0], e[1], ADD, e[2], 0, e[3]],
            );
        }
        p.inc(SUBC_3D, SET_BLEND, &[eq.is_some() as u32]);
        p.inc(SUBC_3D, SET_CT_WRITE, &[0x1111]);
    }

    /// `words` as the shaders' constant buffer, for the draws that follow
    /// (copied now: the batch runs after).
    /// False if the batch has used up its constant buffer memory.
    pub fn uniforms(&mut self, p: &mut Push, words: &[u32]) -> bool {
        self.cbuf(p, UBO_SLOT, words)
    }

    /// `words` as constant buffer `slot` of both shader groups.
    pub fn cbuf(&mut self, p: &mut Push, slot: u8, words: &[u32]) -> bool {
        match self.stage(words) {
            Some((va, bytes)) => {
                self.bind_cbuf(p, slot, va, bytes);
                true
            }
            None => false,
        }
    }

    /// `words` copied to this batch's constant buffer memory: GPU
    /// address and size in bytes (rounded to 256).
    pub fn stage(&mut self, words: &[u32]) -> Option<(u64, u32)> {
        let bytes = words.len() * 4;
        let (at, va) = self.alloc_ubo(bytes)?;
        self.ubo.words(at, words);
        Some((va, bytes.next_multiple_of(256) as u32))
    }

    /// Constant buffer `slot` of both shader groups, from `va`.
    pub fn bind_cbuf(&self, p: &mut Push, slot: u8, va: u64, bytes: u32) {
        p.inc(
            SUBC_3D,
            SET_CONSTANT_BUFFER_SELECTOR_A,
            &[bytes, (va >> 32) as u32, va as u32],
        );
        let bind = (slot as u32) << 4 | 1;
        for g in [GROUP_VERTEX, GROUP_PIXEL] {
            p.inc(SUBC_3D, BIND_GROUP_CONSTANT_BUFFER + g * 0x20, &[bind]);
        }
    }

    /// Caches that may hold what an earlier pass wrote (textures) or the
    /// CPU did (constants), after the engines are idle.
    pub fn barrier(&self, p: &mut Push) {
        p.immd(SUBC_3D, crate::push::WFI, crate::push::WFI_SCOPE_ALL);
        p.inc(SUBC_3D, INVALIDATE_SHADER_CACHES, &[1 | 1 << 4 | 1 << 12]);
        p.inc(SUBC_3D, INVALIDATE_TEXTURE_HEADER_CACHE, &[0]);
        p.inc(SUBC_3D, INVALIDATE_TEXTURE_DATA_CACHE, &[0]);
    }

    fn stream(&self, p: &mut Push, lay: &Layout, vb: u64, vertices: u32) {
        let size = vertices as u64 * lay.stride as u64;
        p.inc(
            SUBC_3D,
            SET_VERTEX_STREAM_A_FORMAT,
            &[lay.stride | 1 << 12, (vb >> 32) as u32, vb as u32],
        );
        let end = vb + size - 1;
        p.inc(
            SUBC_3D,
            SET_VERTEX_STREAM_LIMIT_A_A,
            &[(end >> 32) as u32, end as u32],
        );
        p.inc(SUBC_3D, SET_VERTEX_ATTRIBUTE_A, lay.attrs);
    }

    /// `count` vertices from `first`, of the `vertices` at `vb`, as triangles.
    pub fn draw(&self, p: &mut Push, pipe: Pipe, vb: u64, vertices: u32, first: u32, count: u32) {
        let attrs = pipe.attributes();
        let lay = Layout {
            stride: pipe.stride(),
            attrs: &attrs,
            inst_stride: 0,
        };
        self.draw_layout(p, &lay, vb, vertices, first, count);
    }

    /// `draw` with any vertex layout.
    pub fn draw_layout(
        &self,
        p: &mut Push,
        lay: &Layout,
        vb: u64,
        vertices: u32,
        first: u32,
        count: u32,
    ) {
        self.stream(p, lay, vb, vertices);
        p.inc(SUBC_3D, BEGIN, &[PRIM_TRIANGLES]);
        p.inc(SUBC_3D, SET_VERTEX_ARRAY_START, &[first]);
        p.inc(SUBC_3D, DRAW_VERTEX_ARRAY, &[count]);
        p.inc(SUBC_3D, END, &[0]);
    }

    /// `count` 16-bit indices from `first`, into the `vertices` at `vb`.
    #[allow(clippy::too_many_arguments)]
    pub fn draw_indexed(
        &self,
        p: &mut Push,
        pipe: Pipe,
        vb: u64,
        vertices: u32,
        ib: u64,
        indices: u32,
        first: u32,
        count: u32,
    ) {
        let attrs = pipe.attributes();
        let lay = Layout {
            stride: pipe.stride(),
            attrs: &attrs,
            inst_stride: 0,
        };
        self.draw_indexed_layout(p, &lay, vb, vertices, ib, indices, first, count);
    }

    /// `draw_indexed` with any vertex layout.
    #[allow(clippy::too_many_arguments)]
    pub fn draw_indexed_layout(
        &self,
        p: &mut Push,
        lay: &Layout,
        vb: u64,
        vertices: u32,
        ib: u64,
        indices: u32,
        first: u32,
        count: u32,
    ) {
        self.stream(p, lay, vb, vertices);
        let end = ib + indices as u64 * 2 - 1;
        p.inc(
            SUBC_3D,
            SET_INDEX_BUFFER_A,
            &[
                (ib >> 32) as u32,
                ib as u32,
                (end >> 32) as u32,
                end as u32,
                1, // two bytes
                first,
            ],
        );
        p.inc(SUBC_3D, BEGIN, &[PRIM_TRIANGLES]);
        p.inc(SUBC_3D, DRAW_INDEX_BUFFER, &[count]);
        p.inc(SUBC_3D, END, &[0]);
    }

    /// Stream 1 of the instances' data, advancing one instance at a time.
    fn instance_stream(&self, p: &mut Push, lay: &Layout, inst: &Instances) {
        if lay.inst_stride == 0 {
            return;
        }
        let m = SET_VERTEX_STREAM_A_FORMAT + 16 * INSTANCE_STREAM;
        let va = inst.va;
        p.inc(
            SUBC_3D,
            m,
            &[
                lay.inst_stride | 1 << 12,
                (va >> 32) as u32,
                va as u32,
                1, // frequency: every instance
            ],
        );
        let end = va + inst.count as u64 * lay.inst_stride as u64 - 1;
        p.inc(
            SUBC_3D,
            SET_VERTEX_STREAM_LIMIT_A_A + 8 * INSTANCE_STREAM,
            &[(end >> 32) as u32, end as u32],
        );
        p.immd(
            SUBC_3D,
            SET_VERTEX_STREAM_INSTANCE_A + 4 * INSTANCE_STREAM,
            1,
        );
    }

    /// Stream 1 off again: later draws read stream 0 alone.
    fn end_instances(&self, p: &mut Push, lay: &Layout) {
        if lay.inst_stride != 0 {
            let m = SET_VERTEX_STREAM_A_FORMAT + 16 * INSTANCE_STREAM;
            p.inc(SUBC_3D, m, &[0]);
            p.immd(
                SUBC_3D,
                SET_VERTEX_STREAM_INSTANCE_A + 4 * INSTANCE_STREAM,
                0,
            );
        }
    }

    /// Words `draw_layout_instanced` pushes at most for `n` instances.
    pub const fn instanced_words(n: u32) -> usize {
        64 + 8 * n as usize
    }

    /// `draw_layout` once per instance (Gallium's sequence: a BEGIN per
    /// instance, with the instance counter going on from the second).
    #[allow(clippy::too_many_arguments)]
    pub fn draw_layout_instanced(
        &self,
        p: &mut Push,
        lay: &Layout,
        vb: u64,
        vertices: u32,
        first: u32,
        count: u32,
        inst: &Instances,
    ) {
        self.stream(p, lay, vb, vertices);
        self.instance_stream(p, lay, inst);
        for i in 0..inst.count {
            let next = if i == 0 { 0 } else { INSTANCE_NEXT };
            p.inc(SUBC_3D, BEGIN, &[PRIM_TRIANGLES | next]);
            p.inc(SUBC_3D, SET_VERTEX_ARRAY_START, &[first]);
            p.inc(SUBC_3D, DRAW_VERTEX_ARRAY, &[count]);
            p.inc(SUBC_3D, END, &[0]);
        }
        self.end_instances(p, lay);
    }

    /// `draw_indexed_layout` once per instance.
    #[allow(clippy::too_many_arguments)]
    pub fn draw_indexed_layout_instanced(
        &self,
        p: &mut Push,
        lay: &Layout,
        vb: u64,
        vertices: u32,
        ib: u64,
        indices: u32,
        first: u32,
        count: u32,
        inst: &Instances,
    ) {
        self.stream(p, lay, vb, vertices);
        self.instance_stream(p, lay, inst);
        let end = ib + indices as u64 * 2 - 1;
        p.inc(
            SUBC_3D,
            SET_INDEX_BUFFER_A,
            &[
                (ib >> 32) as u32,
                ib as u32,
                (end >> 32) as u32,
                end as u32,
                1, // two bytes
                first,
            ],
        );
        for i in 0..inst.count {
            let next = if i == 0 { 0 } else { INSTANCE_NEXT };
            p.inc(SUBC_3D, BEGIN, &[PRIM_TRIANGLES | next]);
            p.inc(SUBC_3D, DRAW_INDEX_BUFFER, &[count]);
            p.inc(SUBC_3D, END, &[0]);
        }
        self.end_instances(p, lay);
    }
}
