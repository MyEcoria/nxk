//! nxk-bl31: minimal EL3 monitor launching nxk from hekate's L4T entry (RCM -> hekate -> monitor -> nxk).
//! EL3 per-core setup, UART-A, screen mirror, counter, GIC Group 1, TZ carveout, copy, EL2 jump, PSCI CPU_ON; no DTB/DVFS. Same binary runs under QEMU (see `hwboot/test-bl31.sh`).

#![no_std]
#![no_main]

mod fbcon;
mod psci;

use core::arch::global_asm;
use core::ptr::{read_volatile, write_volatile};

/// nxk.bin size, baked in at compile time by `mkbl31.py` (`NXK_BL33_SIZE`):
/// the monitor trusts nothing from its environment.
fn bl33_size() -> u64 {
    match option_env!("NXK_BL33_SIZE") {
        Some(s) => parse_u64(s),
        None => 0,
    }
}

const BL33_SRC: usize = 0xAA00_0000; // where hekate drops bl33.bin
const BL33_DST: usize = 0x8000_0000; // nxk link address
const MON_BASE: u32 = 0xFFF0_0000; // 1 MiB TZDRAM (non-b01 Erista)

const UART_A: usize = 0x7000_6000;
const UART_CLK_HZ: u32 = 408_000_000;
const CAR_BASE: usize = 0x6000_6000;
const CAR_CLK_SOURCE_UARTA: usize = CAR_BASE + 0x178;
const CAR_RST_DEV_L_SET: usize = CAR_BASE + 0x300;
const CAR_RST_DEV_L_CLR: usize = CAR_BASE + 0x304;
const CAR_CLK_ENB_L_SET: usize = CAR_BASE + 0x320;
const CAR_CLK_ENB_L_CLR: usize = CAR_BASE + 0x324;
const CAR_BIT_UARTA: u32 = 1 << 6;
/// UART-A source: PLLP_OUT0 (bits 31:29 = 0), divider off (bit 24 clear).
const UARTA_SRC_PLLP: u32 = 0;

const GICD: usize = 0x5004_1000;
const GICC: usize = 0x5004_2000;

const MC_SECURITY_CFG0: usize = 0x7001_9070;
const MC_SECURITY_CFG1: usize = 0x7001_9074;

/// System counter (CNTPCT_EL0 source): EN | HDBG, as in exosphere.
const SYSCTR0_CNTCR: usize = 0x700F_0000;

const PMC_BASE: usize = 0x7000_E400;
const PMC_SCRATCH110: usize = PMC_BASE + 0xB10;
const IRAM_MAGIC: u32 = 0x4D41_5249; // "IRAM": hekate params present

const fn parse_u64(s: &str) -> u64 {
    let b = s.as_bytes();
    let mut i = 0;
    let mut v = 0u64;
    while i < b.len() {
        let d = match b[i] {
            c @ b'0'..=b'9' => c - b'0',
            b'_' => {
                i += 1;
                continue;
            }
            _ => panic!("invalid NXK_BL33_SIZE"),
        };
        v = v * 10 + d as u64;
        i += 1;
    }
    v
}

fn wr(a: usize, v: u32) {
    unsafe { write_volatile(a as *mut u32, v) }
}
fn rd(a: usize) -> u32 {
    unsafe { read_volatile(a as *const u32) }
}

/// UART-A at 115200 8N1; enable clock+reset via CAR first (else LSR never moves).
fn uart_init() {
    // The divisor assumes PLLP, but UART-A is found on a ~19.2 MHz source
    // (5.4 kbaud). Switched under reset, as hekate's clock_enable() does.
    wr(CAR_RST_DEV_L_SET, CAR_BIT_UARTA);
    wr(CAR_CLK_ENB_L_CLR, CAR_BIT_UARTA);
    wr(CAR_CLK_SOURCE_UARTA, UARTA_SRC_PLLP);
    wr(CAR_CLK_ENB_L_SET, CAR_BIT_UARTA);
    // hekate waits 2 us here; the counter is not started yet, and 128
    // CAR reads (bus round trips) take longer than that.
    for _ in 0..128 {
        rd(CAR_CLK_SOURCE_UARTA);
    }
    wr(CAR_RST_DEV_L_CLR, CAR_BIT_UARTA);
    // 16550 indexes spaced 4 bytes apart (registers x4).
    let r = |i: usize| UART_A + i * 4;
    wr(r(1), 0);
    wr(r(3), 1 << 7); // DLAB
    let div = (UART_CLK_HZ + 115200 * 8) / (115200 * 16);
    wr(r(0), div & 0xFF);
    wr(r(1), (div >> 8) & 0xFF);
    wr(r(3), 0b11); // 8N1
    wr(r(2), (1 << 0) | (1 << 1) | (1 << 2)); // FIFO on + clear
}

fn putc(c: u8) {
    while rd(UART_A + 5 * 4) & (1 << 5) == 0 {}
    wr(UART_A, c as u32);
}
fn puts(s: &str) {
    for b in s.bytes() {
        if b == b'\n' {
            putc(b'\r');
        }
        putc(b);
    }
    fbcon::write_str(s);
}
fn puthex(v: u64) {
    let mut buf = *b"0x0000000000000000";
    for i in 0..16 {
        let d = ((v >> ((15 - i) * 4)) & 0xF) as u8;
        buf[2 + i] = if d < 10 { b'0' + d } else { b'a' + d - 10 };
    }
    puts(core::str::from_utf8(&buf).unwrap_or("0x?"));
}

/// All lines to Group 1: non-secure EL1 gets its IRQs. Secure-only
/// (IGROUPR): EL3-only, hence the monitor.
fn gic_secure_init() {
    wr(GICD, 0);
    let lines = ((rd(GICD + 4) & 0x1F) + 1) * 32;
    for i in (0..lines).step_by(32) {
        wr(GICD + 0x80 + (i as usize / 32) * 4, 0xFFFF_FFFF);
    }
    wr(GICD, 0b11); // EnableGrp0 | EnableGrp1
    gic_secure_init_cpu();
}

/// Banked per core: SGI/PPI to Group 1, secure CPU interface on.
fn gic_secure_init_cpu() {
    wr(GICD + 0x80, 0xFFFF_FFFF);
    wr(GICC, (1 << 0) | (1 << 1) | (1 << 3));
    wr(GICC + 4, 0xFF);
}

fn park() -> ! {
    loop {
        unsafe { core::arch::asm!("msr daifset, #0xF", "wfe", options(nostack)) };
    }
}

/// Copy `size` bytes then invalidate the instruction cache: we just wrote
/// code we are about to run, MMU off.
fn relocate(src: usize, dst: usize, size: u64) {
    unsafe {
        core::ptr::copy_nonoverlapping(src as *const u8, dst as *mut u8, size as usize);
        core::arch::asm!("dsb sy", "ic ialluis", "dsb ish", "isb", options(nostack));
    }
}

/// Core 0 entry. Secondaries park in assembly.
#[no_mangle]
extern "C" fn bl31_main() -> ! {
    uart_init();
    fbcon::init();
    puts("\n[nxk-bl31] EL3 monitor (hekate L4T entry)\n");

    // No-op if already running; stopped, CNTPCT_EL0 would stay frozen and
    // every bounded nxk wait would last forever.
    wr(SYSCTR0_CNTCR, rd(SYSCTR0_CNTCR) | 0b11);
    puts("[nxk-bl31] system counter on, CNTFRQ = 19.2 MHz\n");

    gic_secure_init();
    puts("[nxk-bl31] GIC: all to Group 1\n");

    // Declare our MiB a secure zone: nxk reads these registers
    // (`detect_holes`) and excludes it from free DRAM. Same pair as l4tboot.
    wr(MC_SECURITY_CFG0, MON_BASE);
    wr(MC_SECURITY_CFG1, 1);
    puts("[nxk-bl31] TZ carveout: 0xfff00000 (1 MiB)\n");

    psci::init();
    puts("[nxk-bl31] PSCI 1.0: CPU_ON for cores 1-3\n");

    if rd(PMC_SCRATCH110) == IRAM_MAGIC {
        puts("[nxk-bl31] hekate params present (IRAM)\n");
    } else {
        puts("[nxk-bl31] no IRAM magic: test launch (emulator)\n");
    }

    let size = bl33_size();
    if size == 0 || size > 8 << 20 {
        puts("[nxk-bl31] ERROR: bad bl33 size (NXK_BL33_SIZE)\n");
        park();
    }
    puts("[nxk-bl31] copying bl33: ");
    puthex(BL33_SRC as u64);
    puts(" -> ");
    puthex(BL33_DST as u64);
    puts(" (");
    puthex(size);
    puts(" bytes)\n");
    relocate(BL33_SRC, BL33_DST, size);
    puts("[nxk-bl31] jumping to EL2, x0 = 0 (no DTB)\n");
    fbcon::handoff();

    unsafe { drop_to_el2(BL33_DST as u64, 0) }
}

extern "C" {
    /// EL3 -> EL2h at `entry` with x0 = `ctx`; per-core EL2 state reset.
    fn drop_to_el2(entry: u64, ctx: u64) -> !;
}

/// PSCI from EL2/EL1 (silent: nxk owns UART and screen after the jump).
#[no_mangle]
extern "C" fn smc_handler(regs: &mut [u64; 31]) {
    psci::smc(regs);
}

/// Trapped access or unexpected exception: print then park.
#[no_mangle]
extern "C" fn el3_unexpected(regs: &mut [u64; 31]) {
    let (esr, elr): (u64, u64);
    unsafe {
        core::arch::asm!("mrs {}, esr_el3", out(reg) esr);
        core::arch::asm!("mrs {}, elr_el3", out(reg) elr);
    }
    puts("\n[nxk-bl31] unexpected EL3 trap, ESR=");
    puthex(esr);
    puts(" ELR=");
    puthex(elr);
    puts("\n");
    let _ = regs;
    park()
}

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    puts("\n[nxk-bl31] panic\n");
    park()
}

global_asm!(
    r#"
.section .text.boot, "ax"
.globl _start
_start:
    // A core reset without a power cycle keeps CPU_OFF's OS double lock.
    msr     osdlr_el1, xzr
    // Per-core EL3 setup (ATF/exosphere values; UNKNOWN reset on silicon): CNTFRQ_EL0,
    // CPUECTLR_EL1.SMPEN, CPUACTLR_EL1[0] (Spectre-v2 IALLU, as in nxk boot.rs).
    movz    x0, #0xF800
    movk    x0, #0x0124, lsl #16
    msr     cntfrq_el0, x0
    mrs     x0, S3_1_C15_C2_1           // CPUECTLR_EL1
    orr     x0, x0, #(1 << 6)
    msr     S3_1_C15_C2_1, x0
    mrs     x0, S3_1_C15_C2_0           // CPUACTLR_EL1
    orr     x0, x0, #1
    msr     S3_1_C15_C2_0, x0
    isb
    mrs     x19, mpidr_el1
    and     x19, x19, #0xFF
    adrp    x0, __stack_top
    add     x0, x0, :lo12:__stack_top
    mov     x1, #0x4000
    msub    x0, x1, x19, x0
    mov     sp, x0
    adrp    x0, el3_vectors
    add     x0, x0, :lo12:el3_vectors
    msr     vbar_el3, x0
    isb
    cbnz    x19, .Lsecondary            // woken by PSCI CPU_ON
    adrp    x0, __bss_start
    add     x0, x0, :lo12:__bss_start
    adrp    x1, __bss_end
    add     x1, x1, :lo12:__bss_end
.Lbss:
    cmp     x0, x1
    b.hs    .Lbss_done
    str     xzr, [x0], #8
    b       .Lbss
.Lbss_done:
    bl      bl31_main
.Lsecondary:
    mov     x0, x19
    bl      bl31_secondary              // does not return

// drop_to_el2(entry, ctx) -> ! : EL3 -> EL2h, ELR = entry, x0 = ctx.
.globl drop_to_el2
drop_to_el2:
    mov     x19, x0
    mov     x20, x1
    // SCR_EL3 = NS | RES1 | SMD=0 | HCE | RW: `smc` traps to EL3 (PSCI).
    mov     x0, #0x531
    msr     scr_el3, x0
    msr     cptr_el3, xzr
    msr     mdcr_el3, xzr
    msr     cntvoff_el2, xzr
    // EL2 in a known state (ATF-like): SCTLR_EL2 RES1-only, no debug/PMU/AArch32
    // traps, no stage-2 translation, hypervisor timer stopped.
    movz    x0, #0x0830
    movk    x0, #0x30C5, lsl #16
    msr     sctlr_el2, x0
    mrs     x0, pmcr_el0
    ubfx    x0, x0, #11, #5
    msr     mdcr_el2, x0
    msr     hstr_el2, xzr
    msr     vttbr_el2, xzr
    msr     cnthp_ctl_el2, xzr
    mov     x0, #1
    lsl     x0, x0, #31                // HCR_EL2.RW: EL1 in AArch64
    msr     hcr_el2, x0
    mov     x0, #3                     // physical timer visible in EL1
    msr     cnthctl_el2, x0
    mrs     x0, midr_el1
    msr     vpidr_el2, x0
    mrs     x0, mpidr_el1
    msr     vmpidr_el2, x0
    mov     x0, #0x33FF
    msr     cptr_el2, x0
    movz    x0, #0x0800
    movk    x0, #0x30D0, lsl #16
    msr     sctlr_el1, x0
    mov     x0, #0x3C9                 // SPSR: EL2h, all masked
    msr     spsr_el3, x0
    msr     elr_el3, x19
    mov     x0, x20                    // boot core: 0 = no DTB
    mov     x1, xzr
    mov     x2, xzr
    mov     x3, xzr
    isb
    eret

// cpu_power_down() -> ! : exosphere's FinalizePowerOff. The flow controller
// is armed: the core's partition is gated at the `wfi`.
.globl cpu_power_down
cpu_power_down:
    mrs     x0, sctlr_el1
    bic     x0, x0, #(1 << 2)
    msr     sctlr_el1, x0
    isb
    mrs     x0, sctlr_el3
    bic     x0, x0, #(1 << 2)
    msr     sctlr_el3, x0
    isb
    // CPUECTLR: no table-walk descriptor prefetch, no instruction or data
    // prefetch, before the flush.
    mrs     x0, S3_1_C15_C2_1
    orr     x0, x0, #(1 << 38)
    bic     x0, x0, #(3 << 35)
    bic     x0, x0, #(3 << 32)
    msr     S3_1_C15_C2_1, x0
    isb
    dsb     sy
    // Clean and invalidate L1 data by set/way (L2 stays: the cluster is on).
    msr     csselr_el1, xzr
    isb
    mrs     x1, ccsidr_el1
    and     x2, x1, #7
    add     x2, x2, #4                 // set shift: log2(line bytes)
    ubfx    x3, x1, #3, #10            // ways - 1
    ubfx    x4, x1, #13, #15           // sets - 1
    clz     w5, w3                     // way shift
.Lway:
    mov     x6, x4
.Lset:
    lsl     x7, x3, x5
    lsl     x8, x6, x2
    orr     x7, x7, x8
    dc      cisw, x7
    subs    x6, x6, #1
    b.ge    .Lset
    subs    x3, x3, #1
    b.ge    .Lway
    dsb     sy
    // Leave coherency (no more broadcast maintenance), then mute the GIC
    // CPU interface (bypass disabled both groups) and lock the OS double lock.
    mrs     x0, S3_1_C15_C2_1
    bic     x0, x0, #(1 << 6)
    msr     S3_1_C15_C2_1, x0
    movz    x1, #0x2000
    movk    x1, #0x5004, lsl #16
    mov     w0, #0x1E0
    str     w0, [x1]
    mrs     x0, osdlr_el1
    orr     x0, x0, #1
    msr     osdlr_el1, x0
    isb
    dsb     sy
.Loff:
    wfi
    b       .Loff

.macro SAVE
    sub     sp, sp, #256
    stp     x0, x1, [sp, #0]
    stp     x2, x3, [sp, #16]
    stp     x4, x5, [sp, #32]
    stp     x6, x7, [sp, #48]
    stp     x8, x9, [sp, #64]
    stp     x10, x11, [sp, #80]
    stp     x12, x13, [sp, #96]
    stp     x14, x15, [sp, #112]
    stp     x16, x17, [sp, #128]
    stp     x18, x19, [sp, #144]
    stp     x20, x21, [sp, #160]
    stp     x22, x23, [sp, #176]
    stp     x24, x25, [sp, #192]
    stp     x26, x27, [sp, #208]
    stp     x28, x29, [sp, #224]
    str     x30, [sp, #240]
.endm
.macro RESTORE
    ldp     x0, x1, [sp, #0]
    ldp     x2, x3, [sp, #16]
    ldp     x4, x5, [sp, #32]
    ldp     x6, x7, [sp, #48]
    ldp     x8, x9, [sp, #64]
    ldp     x10, x11, [sp, #80]
    ldp     x12, x13, [sp, #96]
    ldp     x14, x15, [sp, #112]
    ldp     x16, x17, [sp, #128]
    ldp     x18, x19, [sp, #144]
    ldp     x20, x21, [sp, #160]
    ldp     x22, x23, [sp, #176]
    ldp     x24, x25, [sp, #192]
    ldp     x26, x27, [sp, #208]
    ldp     x28, x29, [sp, #224]
    ldr     x30, [sp, #240]
    add     sp, sp, #256
.endm

.Lsmc_entry:
    SAVE
    mrs     x9, esr_el3
    lsr     x9, x9, #26
    mov     x0, sp
    cmp     x9, #0x17                  // SMC from AArch64?
    b.ne    .Lsmc_bad
    bl      smc_handler
    RESTORE
    eret
.Lsmc_bad:
    bl      el3_unexpected             // does not return
    RESTORE
    eret
.Lbad:
    SAVE
    mov     x0, sp
    bl      el3_unexpected             // does not return
    RESTORE
    eret

.align 11
el3_vectors:
    .align 7
    b .Lbad
    .align 7
    b .Lbad
    .align 7
    b .Lbad
    .align 7
    b .Lbad
    .align 7
    b .Lbad
    .align 7
    b .Lbad
    .align 7
    b .Lbad
    .align 7
    b .Lbad
    // lower level AArch64: sync (smc), irq, fiq, serror
    .align 7
    b .Lsmc_entry
    .align 7
    b .Lbad
    .align 7
    b .Lbad
    .align 7
    b .Lbad
    .align 7
    b .Lbad
    .align 7
    b .Lbad
    .align 7
    b .Lbad
    .align 7
    b .Lbad
"#
);
