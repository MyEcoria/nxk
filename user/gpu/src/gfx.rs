//! The 3D engine (MAXWELL_B, class 0xB197): state, a vertex and a
//! fragment shader with their program headers, clears and draws into a
//! linear render target. Method numbers from NVIDIA's open-gpu-doc
//! (classes/3d/cl9097.h, clb197.h), sequences from Mesa's Gallium nvc0
//! (nvc0_screen.c, nvc0_state_validate.c) and NVK; shader encodings and
//! the program header (SPH version 3, classes/3d/cla097sph.h) as NAK
//! writes them.

extern crate alloc;

use alloc::vec::Vec;

use crate::engine::Surface;
use crate::hw::{Dma, Result};
use crate::mmu::Vm;
use crate::push::{Push, SET_OBJECT, SUBC_3D};
use crate::sass::{ald, ast, exit, ipa, Asm, Interp, Sched, RZ};

pub const MAXWELL_B: u32 = 0xB197;

// Methods (cl9097.h).
const SET_SHADER_LOCAL_MEMORY_WINDOW: u32 = 0x077C;
const SET_RASTER_ENABLE: u32 = 0x037C;
const SET_COLOR_TARGET_A: u32 = 0x0800;
const SET_VIEWPORT_SCALE_X: u32 = 0x0A00;
const SET_VIEWPORT_CLIP_HORIZONTAL: u32 = 0x0C00;
const SET_SCISSOR_ENABLE: u32 = 0x0E00;
const SET_SURFACE_CLIP_HORIZONTAL: u32 = 0x0FF4;
const SET_COLOR_CLEAR_VALUE: u32 = 0x0D80;
const SET_VERTEX_ARRAY_START: u32 = 0x0D74;
const DRAW_VERTEX_ARRAY: u32 = 0x0D78;
const SET_CLEAR_SURFACE_CONTROL: u32 = 0x10F8;
const SET_VERTEX_ATTRIBUTE_A: u32 = 0x1160;
const SET_CT_SELECT: u32 = 0x121C;
const SET_DEPTH_TEST: u32 = 0x12CC;
const SET_BLEND: u32 = 0x1360;
const SET_WINDOW_ORIGIN: u32 = 0x13AC;
const SET_PROGRAM_REGION_A: u32 = 0x1608;
const END: u32 = 0x1614;
const BEGIN: u32 = 0x1618;
const CHECK_SPH_VERSION: u32 = 0x16A8;
const SET_VIEWPORT_SCALE_OFFSET: u32 = 0x192C;
const CLEAR_SURFACE: u32 = 0x19D0;
const SET_CT_WRITE: u32 = 0x1A00;
const SET_ZT_SELECT: u32 = 0x1538;
const SET_VERTEX_STREAM_A_FORMAT: u32 = 0x1C00;
const SET_VERTEX_STREAM_LIMIT_A_A: u32 = 0x1F00;
const SET_PIPELINE_SHADER: u32 = 0x2000;
const PIPELINE_STRIDE: u32 = 0x40;

/// `SET_PIPELINE_SHADER` index = type: vertex, pixel.
const TYPE_VERTEX: u32 = 1;
const TYPE_PIXEL: u32 = 5;
const PRIM_TRIANGLES: u32 = 4;
/// Linear (pitch) layout in `SET_COLOR_TARGET_MEMORY`.
const LAYOUT_PITCH: u32 = 1 << 12;

/// Vertex: position and color, four floats each.
pub const VERTEX_SIZE: u32 = 32;
/// Program header, then code.
const SPH_WORDS: usize = 20;
const SPH_BYTES: usize = SPH_WORDS * 4;
/// Headers at 0x30 modulo 0x80, so that the code that follows (0x50
/// later) starts on a 0x80 boundary, as Kepler and later want.
const VS_AT: usize = 0x30;
const FS_AT: usize = 0x430;
const REGION_SIZE: usize = 0x1000;
/// Registers the shaders use, as `SET_PIPELINE_REGISTER_COUNT` wants.
const GPRS: u32 = 8;

/// Attribute addresses (NAK: 0x70 position, 0x80 + 16 n generic n).
const A_POSITION: u32 = 0x70;
const A_GENERIC0: u32 = 0x80;
const A_GENERIC1: u32 = 0x90;

fn field(w: &mut [u32; SPH_WORDS], lo: usize, v: u32) {
    w[lo / 32] |= v << (lo % 32);
}

/// Vertex shader header: attributes 0 and 1 in, position and generic 0
/// out (NAK `encode_header`).
fn vs_header() -> [u32; SPH_WORDS] {
    let mut h = [0u32; SPH_WORDS];
    field(&mut h, 0, 1); // SPH type: VTG
    field(&mut h, 5, 3); // version
    field(&mut h, 10, 1); // vertex shader
    field(&mut h, 17, 1); // SASS version
    field(&mut h, 140, 0xFF); // no store-request range
    field(&mut h, 192, 0xFF); // attributes 0, 1 in: xyzw each
    field(&mut h, 428, 0xF); // position out
    field(&mut h, 432, 0xF); // generic 0 out
    h
}

/// Fragment shader header: generic 0 in, screen-linear; target 0 out.
fn fs_header() -> [u32; SPH_WORDS] {
    let mut h = [0u32; SPH_WORDS];
    field(&mut h, 0, 2); // SPH type: PS
    field(&mut h, 5, 3);
    field(&mut h, 10, 5); // pixel shader
    field(&mut h, 14, 1); // MRT enable (color 0 broadcast)
    field(&mut h, 17, 1);
    field(&mut h, 191, 1); // reads position.w, as Gallium declares it
    field(&mut h, 192, 0xFF); // generic 0 in, four words, screen-linear (3)
    field(&mut h, 576, 0xF); // target 0 out: RGBA
    h
}

fn vs_code() -> Vec<u64> {
    let mut a = Asm::new();
    // Input attribute n is at 0x80 + 16 n, as outputs are.
    a.emit(ald(0, A_GENERIC0, 4), Sched::stall(1).wr(0));
    a.emit(ald(4, A_GENERIC1, 4), Sched::stall(1).wr(1));
    a.emit(ast(0, A_POSITION, 4), Sched::stall(1).rd(2).wait(1));
    a.emit(ast(4, A_GENERIC0, 4), Sched::stall(1).rd(3).wait(2));
    a.emit(exit(), Sched::stall(5).wait(0x3F));
    a.finish()
}

fn fs_code() -> Vec<u64> {
    let mut a = Asm::new();
    for c in 0..4u8 {
        a.emit(
            ipa(c, A_GENERIC0 + 4 * c as u32, Interp::Pass, RZ),
            Sched::stall(1).wr(0),
        );
    }
    a.emit(exit(), Sched::stall(5).wait(0x3F));
    a.finish()
}

/// Shaders in GPU memory.
pub struct Gfx {
    _prog: Dma,
    prog_va: u64,
}

impl Gfx {
    pub fn new(vm: &mut Vm) -> Result<Self> {
        let prog = Dma::new(REGION_SIZE)?;
        for (at, hdr, code) in [
            (VS_AT, vs_header(), vs_code()),
            (FS_AT, fs_header(), fs_code()),
        ] {
            prog.words(at, &hdr);
            for (i, w) in code.iter().enumerate() {
                prog.w64(at + SPH_BYTES + i * 8, *w);
            }
        }
        let prog_va = vm.map(&prog, false)?;
        Ok(Self {
            _prog: prog,
            prog_va,
        })
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
        p.inc(SUBC_3D, SET_ZT_SELECT, &[0]);
        p.inc(SUBC_3D, SET_DEPTH_TEST, &[0]);
        p.inc(SUBC_3D, SET_BLEND, &[0]);
        // All four channels written.
        p.inc(SUBC_3D, SET_CT_WRITE, &[0x1111]);
        // Shaders: vertex (type 1), pixel (type 5), each at its header.
        for (ty, at, bind) in [(TYPE_VERTEX, VS_AT, 0), (TYPE_PIXEL, FS_AT, 4)] {
            let m = SET_PIPELINE_SHADER + ty * PIPELINE_STRIDE;
            // Shader (enabled, type), program offset; register count and
            // binding group after the reserved word.
            p.inc(SUBC_3D, m, &[1 | ty << 4, at as u32]);
            p.inc(SUBC_3D, m + 0xC, &[GPRS, bind]);
        }
    }

    /// The render target and its viewport.
    fn target(&self, p: &mut Push, s: &Surface) {
        p.inc(
            SUBC_3D,
            SET_SURFACE_CLIP_HORIZONTAL,
            &[s.width << 16, s.height << 16],
        );
        p.inc(
            SUBC_3D,
            SET_COLOR_TARGET_A,
            &[
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
        );
        p.inc(SUBC_3D, SET_CT_SELECT, &[1]);
        let (w, h) = (s.width as f32, s.height as f32);
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
                s.width << 16,
                s.height << 16,
                0.0f32.to_bits(),
                1.0f32.to_bits(),
            ],
        );
        p.inc(
            SUBC_3D,
            SET_SCISSOR_ENABLE,
            &[1, s.width << 16, s.height << 16],
        );
    }

    /// Whole target set to `rgba`.
    pub fn clear(&self, p: &mut Push, s: &Surface, rgba: [f32; 4]) {
        self.target(p, s);
        p.inc(SUBC_3D, SET_COLOR_CLEAR_VALUE, &rgba.map(|c| c.to_bits()));
        p.inc(SUBC_3D, SET_CLEAR_SURFACE_CONTROL, &[0]);
        p.inc(SUBC_3D, CLEAR_SURFACE, &[0xF << 2]);
    }

    /// `count` vertices of `VERTEX_SIZE` bytes at `vb` as triangles.
    pub fn draw(&self, p: &mut Push, s: &Surface, vb: u64, count: u32) {
        self.target(p, s);
        let size = count as u64 * VERTEX_SIZE as u64;
        p.inc(
            SUBC_3D,
            SET_VERTEX_STREAM_A_FORMAT,
            &[VERTEX_SIZE | 1 << 12, (vb >> 32) as u32, vb as u32],
        );
        let end = vb + size - 1;
        p.inc(
            SUBC_3D,
            SET_VERTEX_STREAM_LIMIT_A_A,
            &[(end >> 32) as u32, end as u32],
        );
        // Four 32-bit floats each (component widths 1, FLOAT numeric).
        let attr = |offset: u32| 1 << 21 | 7 << 27 | offset << 7;
        p.inc(SUBC_3D, SET_VERTEX_ATTRIBUTE_A, &[attr(0), attr(16)]);
        p.inc(SUBC_3D, BEGIN, &[PRIM_TRIANGLES]);
        p.inc(SUBC_3D, SET_VERTEX_ARRAY_START, &[0]);
        p.inc(SUBC_3D, DRAW_VERTEX_ARRAY, &[count]);
        p.inc(SUBC_3D, END, &[0]);
    }
}
