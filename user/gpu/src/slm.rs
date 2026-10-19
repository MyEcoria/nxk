//! Local memory for shaders: a reserved area every lane of every warp
//! has `BYTES_PER_LANE` of, which the classes find through
//! `SET_SHADER_LOCAL_MEMORY_*` (NVK `nvk_slm_area_ensure` and
//! `nvk_queue_state_update`). A shader declares how much it uses: the
//! program header (graphics) or the QMD (compute) says.

extern crate alloc;

use crate::hw::{Dma, Result};
use crate::mmu::Vm;
use crate::push::{Push, SUBC_3D, SUBC_COMPUTE};

/// Bytes of local memory per lane: the IR's `MAX_LOCAL_WORDS` words.
pub const BYTES_PER_LANE: u32 = 256;
/// Bytes per warp for the reconvergence stack past its on-chip entries
/// (the shaders' headers and QMDs declare it).
pub const CRS_BYTES_PER_WARP: u32 = nxk_shader::lower::CRS_BYTES_PER_WARP;
/// Resident warps per SM on a GM20B.
const WARPS_PER_SM: u64 = 64;

// 3D class: area address (high, low), its size (high, low), bytes per warp.
const SET_SHADER_LOCAL_MEMORY_A: u32 = 0x0790;
// Compute class: the same address, and the size per TPC for the two
// scheduling modes.
const SET_SHADER_LOCAL_MEMORY_NON_THROTTLED_A: u32 = 0x02E4;
const SET_SHADER_LOCAL_MEMORY_THROTTLED_A: u32 = 0x02F0;

pub struct Slm {
    _mem: Dma,
    va: u64,
    size: u64,
    per_warp: u64,
    per_tpc: u64,
}

impl Slm {
    /// The area for a GPU of `tpc_count` TPCs (one SM each).
    pub fn new(vm: &mut Vm, tpc_count: u64) -> Result<Self> {
        // The hardware wants these alignments (NVK's comments).
        let per_warp = (BYTES_PER_LANE as u64 * 32 + CRS_BYTES_PER_WARP as u64).next_multiple_of(0x200);
        let per_tpc = (per_warp * WARPS_PER_SM).next_multiple_of(0x8000);
        let size = (per_tpc * tpc_count).next_multiple_of(0x20000);
        let mem = Dma::new(size as usize)?;
        let va = vm.map(&mem, false)?;
        Ok(Self {
            _mem: mem,
            va,
            size,
            per_warp,
            per_tpc,
        })
    }

    pub fn setup(&self, p: &mut Push) {
        let (hi, lo) = ((self.va >> 32) as u32, self.va as u32);
        p.inc(
            SUBC_3D,
            SET_SHADER_LOCAL_MEMORY_A,
            &[
                hi,
                lo,
                (self.size >> 32) as u32,
                self.size as u32,
                self.per_warp as u32,
            ],
        );
        p.inc(SUBC_COMPUTE, SET_SHADER_LOCAL_MEMORY_A, &[hi, lo]);
        let per_tpc = [(self.per_tpc >> 32) as u32, self.per_tpc as u32, 0xFF];
        p.inc(
            SUBC_COMPUTE,
            SET_SHADER_LOCAL_MEMORY_NON_THROTTLED_A,
            &per_tpc,
        );
        p.inc(SUBC_COMPUTE, SET_SHADER_LOCAL_MEMORY_THROTTLED_A, &per_tpc);
    }
}
