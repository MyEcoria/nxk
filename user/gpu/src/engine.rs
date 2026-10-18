//! Work for the graphics engine: the 2D class (FERMI_TWOD_A) for fills
//! and copies, the compute class (MAXWELL_COMPUTE_B) for the built-in
//! kernels (Mesa: nvc0_surface.c, NVK nvk_cmd_dispatch.c, NAK hw_runner.rs).

extern crate alloc;

use alloc::vec::Vec;

use crate::arena::Arena;
use crate::hw::{Dma, Result};
use crate::kernels;
use crate::mmu::{Vm, PAGE};
use crate::push::{Push, SET_OBJECT, SUBC_2D, SUBC_COMPUTE};
use crate::qmd::{Launch, Qmd};

const FERMI_TWOD_A: u32 = 0x902D;
const MAXWELL_COMPUTE_B: u32 = 0xB1C0;

// 2D methods (open-gpu-doc classes/twod/cl902d.h).
const DST_FORMAT: u32 = 0x200;
const DST_PITCH: u32 = 0x214;
const SRC_FORMAT: u32 = 0x230;
const SRC_PITCH: u32 = 0x244;
const CLIP_ENABLE: u32 = 0x290;
const OPERATION: u32 = 0x2AC;
const OPERATION_SRCCOPY: u32 = 3;
const RENDER_TO_ZETA: u32 = 0x2B8;
const SOLID_PRIM_MODE: u32 = 0x580;
const SOLID_RECTS: u32 = 4;
const SOLID_POINT: u32 = 0x600;
const SAFE_OVERLAP: u32 = 0x888;
const SAMPLE_MODE: u32 = 0x88C;
const BLIT_DST_X: u32 = 0x8B0;
const BLIT_DU_DX: u32 = 0x8C0;
const BLIT_SRC_X: u32 = 0x8D0;
/// Pitch (linear) surfaces.
const LAYOUT_PITCH: u32 = 1;

// Compute methods (classes/compute/clb1c0.h).
const SHARED_MEMORY_WINDOW: u32 = 0x214;
const INVALIDATE_SKED_CACHES: u32 = 0x298;
const SEND_PCAS_A: u32 = 0x2B4;
const SEND_SIGNALING_PCAS_B: u32 = 0x2BC;
const PCAS_INVALIDATE_SCHEDULE: u32 = 3;
const LOCAL_MEMORY_WINDOW: u32 = 0x77C;
const PROGRAM_REGION_A: u32 = 0x1608;
const SET_TEX_SAMPLER_POOL_A: u32 = 0x155C;
const SET_TEX_HEADER_POOL_A: u32 = 0x1574;

/// A linear surface in GPU memory.
#[derive(Clone, Copy)]
pub struct Surface {
    pub va: u64,
    pub width: u32,
    pub height: u32,
    pub pitch: u32,
    pub format: u32,
}

impl Surface {
    pub fn bytes(&self) -> u64 {
        self.pitch as u64 * self.height as u64
    }
}

fn target(p: &mut Push, mthd: u32, pitch: u32, s: &Surface) {
    p.inc(SUBC_2D, mthd, &[s.format, LAYOUT_PITCH]);
    p.inc(
        SUBC_2D,
        pitch,
        &[s.pitch, s.width, s.height, (s.va >> 32) as u32, s.va as u32],
    );
}

/// Solid rectangle (`NVC0EXASolid`: rectangles, SRCCOPY).
pub fn fill(p: &mut Push, s: &Surface, x: u32, y: u32, w: u32, h: u32, color: u32) {
    target(p, DST_FORMAT, DST_PITCH, s);
    p.inc(SUBC_2D, RENDER_TO_ZETA, &[0]);
    p.inc(SUBC_2D, CLIP_ENABLE, &[0]);
    p.inc(SUBC_2D, OPERATION, &[OPERATION_SRCCOPY]);
    p.inc(SUBC_2D, SOLID_PRIM_MODE, &[SOLID_RECTS, s.format, color]);
    // The second point's Y starts the draw.
    p.inc(SUBC_2D, SOLID_POINT, &[x, y, x + w, y + h]);
}

/// Unscaled copy (`nvc0_2d_texture_do_copy`); the engine sorts out
/// overlap when told the source may be the destination.
#[allow(clippy::too_many_arguments)]
pub fn copy(
    p: &mut Push,
    d: &Surface,
    s: &Surface,
    dx: u32,
    dy: u32,
    sx: u32,
    sy: u32,
    w: u32,
    h: u32,
) {
    target(p, DST_FORMAT, DST_PITCH, d);
    target(p, SRC_FORMAT, SRC_PITCH, s);
    p.inc(SUBC_2D, RENDER_TO_ZETA, &[0]);
    p.inc(SUBC_2D, CLIP_ENABLE, &[0]);
    p.inc(SUBC_2D, OPERATION, &[OPERATION_SRCCOPY]);
    p.inc(SUBC_2D, SAFE_OVERLAP, &[(d.va == s.va) as u32]);
    p.inc(SUBC_2D, SAMPLE_MODE, &[0]);
    p.inc(SUBC_2D, BLIT_DST_X, &[dx, dy, w, h]);
    p.inc(SUBC_2D, BLIT_DU_DX, &[0, 1, 0, 1]);
    // The source Y starts the copy.
    p.inc(SUBC_2D, BLIT_SRC_X, &[0, sx, 0, sy]);
}

/// Built-in kernels, in program-region order.
#[derive(Clone, Copy)]
pub enum Kernel {
    Fill = 0,
    Copy = 1,
    Saxpy = 2,
    Burn = 3,
    Clock = 4,
}

/// Kernel code and the per-launch argument page (constant buffer 0,
/// then the QMD).
pub struct Compute {
    code: Dma,
    code_va: u64,
    offsets: [u32; 5],
    args: Dma,
    args_va: u64,
    /// Room past the built-in kernels for the client's shaders.
    arena: Arena,
}

/// Bytes of program region for the client's compute shaders.
const USER_HEAP: usize = 0x40000;

const ARGS_QMD: usize = 0x100;

impl Compute {
    pub fn new(vm: &mut Vm) -> Result<Self> {
        let programs = [
            kernels::fill(),
            kernels::copy(),
            kernels::saxpy(),
            kernels::burn(),
            kernels::clock(),
        ];
        let mut words: Vec<u64> = Vec::new();
        let mut offsets = [0u32; 5];
        for (k, prog) in programs.iter().enumerate() {
            offsets[k] = (words.len() * 8) as u32;
            words.extend_from_slice(prog);
            // 256-byte aligned programs.
            while !words.len().is_multiple_of(32) {
                words.push(0);
            }
        }
        // Room for the instruction prefetch past the last one.
        words.extend_from_slice(&[0; 32]);
        let builtin = words.len() * 8;
        let code = Dma::new(builtin + USER_HEAP)?;
        for (i, w) in words.iter().enumerate() {
            code.w64(i * 8, *w);
        }
        let code_va = vm.map(&code, false)?;
        let args = Dma::new(PAGE as usize)?;
        let args_va = vm.map(&args, false)?;
        Ok(Self {
            code,
            code_va,
            offsets,
            args,
            args_va,
            arena: Arena::new(builtin as u32, USER_HEAP as u32),
        })
    }

    /// Install a client's compute shader; its program offset and the
    /// block to give back.
    pub fn install(&mut self, code: &[u64]) -> Option<(u32, u32)> {
        // 256-byte aligned, with room for the prefetch past the end.
        let len = (code.len() * 8 + 0x100).next_multiple_of(0x100) as u32;
        let at = self.arena.alloc(len, 0x100)?;
        for (i, w) in code.iter().enumerate() {
            self.code.w64(at as usize + i * 8, *w);
        }
        Some((at, len))
    }

    pub fn uninstall(&mut self, at: u32, len: u32) {
        self.arena.release(at, len);
    }

    /// Launch the QMD at GPU address `qmd_va` (256-aligned).
    pub fn launch(&self, p: &mut Push, qmd_va: u64) {
        p.immd(SUBC_COMPUTE, INVALIDATE_SKED_CACHES, 0);
        p.inc(SUBC_COMPUTE, SEND_PCAS_A, &[(qmd_va >> 8) as u32]);
        p.inc(
            SUBC_COMPUTE,
            SEND_SIGNALING_PCAS_B,
            &[PCAS_INVALIDATE_SCHEDULE],
        );
    }

    /// Channel setup: both classes bound, compute's program region and
    /// memory windows (NVK `nvk_push_dispatch_state_init`).
    pub fn setup(&self, p: &mut Push, pools: (u64, u64, u32)) {
        p.inc(SUBC_2D, SET_OBJECT, &[FERMI_TWOD_A]);
        p.inc(SUBC_COMPUTE, SET_OBJECT, &[MAXWELL_COMPUTE_B]);
        p.inc(
            SUBC_COMPUTE,
            PROGRAM_REGION_A,
            &[(self.code_va >> 32) as u32, self.code_va as u32],
        );
        p.inc(SUBC_COMPUTE, LOCAL_MEMORY_WINDOW, &[0xFF << 24]);
        p.inc(SUBC_COMPUTE, SHARED_MEMORY_WINDOW, &[0xFE << 24]);
        // The texture pools the 3D class samples through: a compute shader
        // fetches texels with the same handles.
        let (headers, samplers, last) = pools;
        p.inc(
            SUBC_COMPUTE,
            SET_TEX_SAMPLER_POOL_A,
            &[(samplers >> 32) as u32, samplers as u32, last],
        );
        p.inc(
            SUBC_COMPUTE,
            SET_TEX_HEADER_POOL_A,
            &[(headers >> 32) as u32, headers as u32, last],
        );
    }

    /// `k` over `threads` threads with constant buffer `cb`; one launch
    /// per batch (the argument page is reused).
    pub fn dispatch(&self, p: &mut Push, k: Kernel, cb: &[u32], threads: u32) {
        self.args.words(0, cb);
        let q = Qmd::new(&Launch {
            program: self.offsets[k as usize],
            regs: kernels::REGS,
            grid: threads.div_ceil(kernels::BLOCK).max(1),
            block: kernels::BLOCK,
            cb0: self.args_va,
            cb0_size: kernels::CB_SIZE,
        });
        self.args.write(ARGS_QMD, &q.bytes());
        p.immd(SUBC_COMPUTE, INVALIDATE_SKED_CACHES, 0);
        p.inc(
            SUBC_COMPUTE,
            SEND_PCAS_A,
            &[((self.args_va + ARGS_QMD as u64) >> 8) as u32],
        );
        p.inc(
            SUBC_COMPUTE,
            SEND_SIGNALING_PCAS_B,
            &[PCAS_INVALIDATE_SCHEDULE],
        );
    }
}

/// Constant buffer for `kernels`: x, y, count, value, loops.
pub fn args(x: u64, y: u64, count: u32, value: u32, loops: u32) -> [u32; 7] {
    [
        x as u32,
        (x >> 32) as u32,
        y as u32,
        (y >> 32) as u32,
        count,
        value,
        loops,
    ]
}
