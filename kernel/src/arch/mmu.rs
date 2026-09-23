//! AArch64 page tables (4 KiB, 39-bit VA), identity-mapped.
//! Only present DRAM is mapped; kernel sections get per-section rights.

use crate::arch;
use crate::{sysreg_read, sysreg_write};

// --- MAIR indices ---
pub const ATTR_DEVICE_NGNRE: u64 = 0;
pub const ATTR_NORMAL_WB: u64 = 1;
pub const ATTR_NORMAL_NC: u64 = 2;
const MAIR_VALUE: u64 = 0x04 | (0xFF << 8) | (0x44 << 16);

// --- Descriptor bits ---
pub const VALID: u64 = 1 << 0;
pub const TABLE: u64 = 1 << 1;
pub const AF: u64 = 1 << 10;
pub const SH_INNER: u64 = 0b11 << 8;
pub const AP_RW_EL1: u64 = 0b00 << 6;
pub const AP_RO_EL1: u64 = 0b10 << 6;
pub const PXN: u64 = 1 << 53;
pub const UXN: u64 = 1 << 54;

pub const fn attr(idx: u64) -> u64 {
    idx << 2
}

pub const PAGE_SIZE: usize = 4096;
pub const ENTRIES: usize = 512;
pub const L1_SIZE: usize = 1 << 30;
pub const L2_SIZE: usize = 2 << 20;

pub const DRAM_BASE: usize = 0x8000_0000;
/// MC_EMEM_CFG: DRAM size in MiB (bits 13:0), written by the bootloader
/// when initializing SDRAM.
const MC_EMEM_CFG: usize = 0x7001_9050;

static mut DRAM_END: usize = DRAM_BASE + L1_SIZE;

#[repr(C, align(4096))]
struct Table([u64; ENTRIES]);

impl Table {
    const fn zeroed() -> Self {
        Self([0; ENTRIES])
    }
    #[inline]
    fn addr(&self) -> u64 {
        self.0.as_ptr() as u64
    }
}

// L1: covers 512 GiB. Only the first entries are used.
static mut L1: Table = Table::zeroed();
// L2 for the first two gigabytes (devices).
static mut L2_MMIO: [Table; 2] = [Table::zeroed(), Table::zeroed()];
// L2 for the first gigabyte of DRAM.
static mut L2_DRAM0: Table = Table::zeroed();
// L3 for the first 2 MiB of DRAM: the kernel image.
static mut L3_KERNEL: Table = Table::zeroed();
// L2 pool for partial gigabytes or ones cut by a reserved region.
const L2_POOL: usize = 4;
static mut L2_DRAM_POOL: [Table; L2_POOL] = [const { Table::zeroed() }; L2_POOL];

/// Monitor/loader-reserved DRAM (carveouts), 2 MiB-aligned, never mapped.
/// Non-secure speculative access there aborts; see refs/hekate `launch_l4t`.
pub const MAX_HOLES: usize = 8;

pub fn detect_holes() -> [(u64, u64); MAX_HOLES] {
    const MC: usize = 0x7001_9000;
    let rd = |off: usize| unsafe { core::ptr::read_volatile((MC + off) as *const u32) } as u64;
    let mut zones = [(0u64, 0u64); MAX_HOLES];
    // TZ-DRAM: MC_SECURITY_CFG0 (base, MiB), CFG3 (bits 33:32), CFG1 (size, MiB).
    zones[0] = (
        ((rd(0x9BC) & 3) << 32) | (rd(0x70) & 0xFFF0_0000),
        (rd(0x74) & 0x3FFF) << 20,
    );
    // Secure carveout: MC_SEC_CARVEOUT_BOM / ADR_HI / SIZE_MB.
    zones[1] = (
        ((rd(0x9D4) & 3) << 32) | (rd(0x670) & 0xFFF0_0000),
        (rd(0x674) & 0xFFF) << 20,
    );
    // VPR: MC_VIDEO_PROTECT_BOM / BOM_ADR_HI / SIZE_MB.
    zones[2] = (
        ((rd(0x978) & 3) << 32) | (rd(0x648) & 0xFFF0_0000),
        (rd(0x64C) & 0x3FFF) << 20,
    );
    // GSC n: MC_SECURITY_CARVEOUTn_BOM / BOM_HI / SIZE_128KB, 0x50 stride.
    for n in 0..5 {
        let r = 0xC0C + n * 0x50;
        zones[3 + n] = (
            ((rd(r + 4) & 3) << 32) | (rd(r) & !0x1_FFFF),
            (rd(r + 8) & 0xFFF) << 17,
        );
    }
    let mut holes = [(0u64, 0u64); MAX_HOLES];
    for (h, (b, sz)) in holes.iter_mut().zip(zones) {
        if sz != 0 {
            *h = round_hole(b, sz);
        }
    }
    holes
}

/// Reserved [base, base+size) rounded out to enclosing 2 MiB blocks.
pub fn round_hole(base: u64, size: u64) -> (u64, u64) {
    let m = L2_SIZE as u64 - 1;
    (base & !m, (base + size + m) & !m)
}

fn in_hole(holes: &[(u64, u64)], pa: u64) -> bool {
    holes.iter().any(|&(b, e)| pa >= b && pa < e)
}

extern "C" {
    static __text_start: u8;
    static __text_end: u8;
    static __rodata_start: u8;
    static __rodata_end: u8;
    static __kernel_end: u8;
}

#[inline]
fn sym(s: &'static u8) -> usize {
    s as *const u8 as usize
}

/// Origin of the retained DRAM size.
pub enum DramSource {
    /// MC_EMEM_CFG, confirmed by probing.
    Controller,
    /// MC_EMEM_CFG claimed more than physically present (emulator: the
    /// register keeps its 8 GiB reset value).
    Probed { announced_mb: usize },
}

/// DRAM size from MC_EMEM_CFG, probed per gigabyte.
/// Call with MMU off; probed words are restored.
pub fn detect_dram() -> (usize, DramSource) {
    let mb = unsafe { core::ptr::read_volatile(MC_EMEM_CFG as *const u32) } as usize & 0x3FFF;
    // Blank register: probe up to the Tegra X1 maximum.
    let announced = if mb == 0 {
        8192
    } else {
        mb.clamp(1024, 8192) & !1
    };
    let announced_bytes = announced << 20;
    let mut size = L1_SIZE;
    while size < announced_bytes {
        let probe = (size + L2_SIZE).min(announced_bytes) - L2_SIZE;
        if !probe_word(DRAM_BASE + probe) {
            break;
        }
        size = (size + L1_SIZE).min(announced_bytes);
    }
    if size == announced_bytes && mb != 0 {
        (size, DramSource::Controller)
    } else {
        (size, DramSource::Probed { announced_mb: mb })
    }
}

fn probe_word(pa: usize) -> bool {
    let p = pa as *mut u64;
    let pattern = 0x6E78_6B5F_6472_616D ^ pa as u64;
    unsafe {
        let old = core::ptr::read_volatile(p);
        core::ptr::write_volatile(p, pattern);
        let a = core::ptr::read_volatile(p);
        core::ptr::write_volatile(p, !pattern);
        let b = core::ptr::read_volatile(p);
        core::ptr::write_volatile(p, old);
        a == pattern && b == !pattern
    }
}

/// End (excluded) of mapped DRAM.
pub fn dram_end() -> u64 {
    unsafe { DRAM_END as u64 }
}

/// Build the tables. Run once, from core 0.
pub unsafe fn build_tables(dram_size: usize, holes: &[(u64, u64)]) {
    let l1 = unsafe { &mut *core::ptr::addr_of_mut!(L1) };
    let l2_mmio = unsafe { &mut *core::ptr::addr_of_mut!(L2_MMIO) };
    let l2_dram = unsafe { &mut *core::ptr::addr_of_mut!(L2_DRAM0) };
    let l3 = unsafe { &mut *core::ptr::addr_of_mut!(L3_KERNEL) };

    // --- 0-2 GiB: device registers, 2 MiB blocks ---
    for (i, table) in l2_mmio.iter_mut().enumerate() {
        for (j, e) in table.0.iter_mut().enumerate() {
            let pa = (i * L1_SIZE + j * L2_SIZE) as u64;
            *e = pa | VALID | AF | attr(ATTR_DEVICE_NGNRE) | AP_RW_EL1 | PXN | UXN;
        }
        l1.0[i] = table.addr() | VALID | TABLE;
    }

    // --- First DRAM GiB: 2 MiB blocks, except the first 2 MiB ---
    for (j, e) in l2_dram.0.iter_mut().enumerate() {
        let pa = (DRAM_BASE + j * L2_SIZE) as u64;
        *e = if in_hole(holes, pa) {
            0
        } else {
            pa | VALID | AF | SH_INNER | attr(ATTR_NORMAL_WB) | AP_RW_EL1 | PXN | UXN
        };
    }

    // --- Kernel image: 4 KiB pages with per-section rights ---
    let text_start = sym(unsafe { &__text_start });
    let text_end = sym(unsafe { &__text_end });
    let rodata_start = sym(unsafe { &__rodata_start });
    let rodata_end = sym(unsafe { &__rodata_end });
    let kernel_end = sym(unsafe { &__kernel_end });
    // Only code must fit in the first 2 MiB (executable 4 KiB pages).
    // Data and .bss beyond that fall in RW non-executable 2 MiB blocks.
    let _ = kernel_end;
    assert!(
        text_end <= DRAM_BASE + L2_SIZE,
        "kernel code > 2 MiB: extend L3_KERNEL"
    );

    for (j, e) in l3.0.iter_mut().enumerate() {
        let va = DRAM_BASE + j * PAGE_SIZE;
        let base = va as u64 | VALID | TABLE | AF | SH_INNER | attr(ATTR_NORMAL_WB) | UXN;
        *e = if va >= text_start && va < text_end {
            base | AP_RO_EL1
        } else if va >= rodata_start && va < rodata_end {
            base | AP_RO_EL1 | PXN
        } else {
            base | AP_RW_EL1 | PXN
        };
    }
    l2_dram.0[0] = l3.addr() | VALID | TABLE;
    l1.0[DRAM_BASE / L1_SIZE] = l2_dram.addr() | VALID | TABLE;

    // --- Rest of DRAM: 1 GiB blocks; 2 MiB blocks for a partial or
    // reserved-region gigabyte ---
    let end = DRAM_BASE + dram_size;
    unsafe { DRAM_END = end };
    let normal = VALID | AF | SH_INNER | attr(ATTR_NORMAL_WB) | AP_RW_EL1 | PXN | UXN;
    let pool = unsafe { &mut *core::ptr::addr_of_mut!(L2_DRAM_POOL) };
    let mut used = 0;
    let mut pa = DRAM_BASE + L1_SIZE;
    while pa < end {
        let whole = pa + L1_SIZE <= end;
        let holed = holes
            .iter()
            .any(|&(b, e)| b < (pa + L1_SIZE) as u64 && e > pa as u64);
        if whole && !holed {
            l1.0[pa / L1_SIZE] = pa as u64 | normal;
        } else {
            assert!(used < L2_POOL, "too many holed gigabytes");
            let t = &mut pool[used];
            used += 1;
            for (j, e) in t.0.iter_mut().enumerate() {
                let b = (pa + j * L2_SIZE) as u64;
                *e = if b < end as u64 && !in_hole(holes, b) {
                    b | normal
                } else {
                    0
                };
            }
            l1.0[pa / L1_SIZE] = t.addr() | VALID | TABLE;
        }
        pa += L1_SIZE;
    }

    arch::dsb_sy();
}

/// Enable the MMU and caches on the current core.
pub unsafe fn enable() {
    let l1_addr = unsafe { (*core::ptr::addr_of!(L1)).addr() };

    sysreg_write!("mair_el1", MAIR_VALUE);

    // IPS from ID_AA64MMFR0_EL1 to match the hardware.
    let pa_range = sysreg_read!("id_aa64mmfr0_el1") & 0xF;
    // TG0 (bits 15:14) = 0: 4 KiB granule.
    let tcr: u64 = 25               // T0SZ: 39-bit VA
        | (0b11 << 12)              // SH0: inner-shareable
        | (0b01 << 10)              // ORGN0: WB WA
        | (0b01 << 8)               // IRGN0: WB WA
        | (1 << 23)                 // EPD1: no walk via TTBR1
        | (pa_range << 32)          // IPS
        | (asid16() << 36)          // AS: 16-bit ASID
        | (25 << 16); // T1SZ (unused, legal value)
    sysreg_write!("tcr_el1", tcr);
    sysreg_write!("ttbr0_el1", l1_addr);

    arch::isb();
    unsafe { core::arch::asm!("tlbi vmalle1", "dsb ish", "isb", options(nostack)) };

    // SCTLR_EL1: MMU (M) + data cache (C) + instruction cache (I).
    let mut sctlr = sysreg_read!("sctlr_el1");
    sctlr |= (1 << 0) | (1 << 2) | (1 << 12);
    sctlr &= !(1 << 1);
    sysreg_write!("sctlr_el1", sctlr);
    arch::isb();
}

/// L1 entries reserved for the kernel (0-16 GiB identity view, enough for
/// 8 GiB DRAM), copied into each user address space. USER_MIN starts after.
pub const KERNEL_L1_ENTRIES: usize = 16;
const _: () = assert!((KERNEL_L1_ENTRIES as u64) << 30 == nxk_abi::layout::USER_MIN);

/// 16-bit ASID if the core supports it (Cortex-A57: yes).
fn asid16() -> u64 {
    u64::from((sysreg_read!("id_aa64mmfr0_el1") >> 4) & 0xF == 2)
}

/// Physical address of the kernel L1 table.
pub fn boot_l1() -> u64 {
    unsafe { (*core::ptr::addr_of!(L1)).addr() }
}

/// Kernel-only TTBR0_EL1 value (ASID 0), used by idle cores.
pub fn kernel_ttbr() -> u64 {
    boot_l1()
}
