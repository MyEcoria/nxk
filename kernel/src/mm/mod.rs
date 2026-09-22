pub mod pmm;
pub mod uaccess;
pub mod vm;

use crate::mmio::{modify32, read32};

/// AHB_AHB_MEM_PREFETCH_CFG3, 4, 1, 2 (AHB gizmo, hekate bdk/soc/t210.h).
const AHB_PREFETCH_CFG: [usize; 4] = [0x6000_C0E4, 0x6000_C0E8, 0x6000_C0F0, 0x6000_C0F4];
const PREFETCH_ENABLE: u32 = 1 << 31;

/// AHB memory prefetch off: a device re-reading an uncached `shm` buffer the
/// CPU rewrote could get stale data (hekate flushes it before each transfer).
/// Returns how many prefetchers were on.
pub fn ahb_prefetch_off() -> usize {
    let mut on = 0;
    for r in AHB_PREFETCH_CFG {
        unsafe {
            if read32(r) & PREFETCH_ENABLE != 0 {
                modify32(r, PREFETCH_ENABLE, 0);
                on += 1;
            }
        }
    }
    on
}
