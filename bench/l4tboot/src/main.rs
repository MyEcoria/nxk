//! l4tboot: minimal EL3 monitor booting L4T (Linux 4.9) on emulated Tegra X1, no hekate/ATF.
//! Switches GIC to non-secure, sets up EL2, jumps to the kernel (x0 = DTB), stays resident for PSCI.

#![no_std]
#![no_main]

use core::arch::global_asm;
use core::ptr::{read_volatile, write_volatile};

/// Payload: Linux by default; build with `L4TBOOT_ENTRY=0x80000000
/// L4TBOOT_EL=2` to launch nxk in EL2 (PSCI path).
const KERNEL_ENTRY: u64 = parse_hex(option_env!("L4TBOOT_ENTRY"), 0x8008_0000);
const ENTRY_EL2: bool = match option_env!("L4TBOOT_EL") {
    Some(s) => s.len() == 1 && s.as_bytes()[0] == b'2',
    None => false,
};
const DTB_ADDR: u64 = 0x8D00_0000;
/// Monitor area declared to the memory controller (TZ-DRAM), as ATF does.
const MC_SECURITY_CFG0: usize = 0x7001_9070;
const MC_SECURITY_CFG1: usize = 0x7001_9074;

const fn parse_hex(s: Option<&str>, default: u64) -> u64 {
    let Some(s) = s else { return default };
    let b = s.as_bytes();
    let mut i = if b.len() > 2 && b[0] == b'0' && (b[1] == b'x' || b[1] == b'X') { 2 } else { 0 };
    let mut v = 0u64;
    while i < b.len() {
        let d = match b[i] {
            c @ b'0'..=b'9' => c - b'0',
            c @ b'a'..=b'f' => c - b'a' + 10,
            c @ b'A'..=b'F' => c - b'A' + 10,
            b'_' => {
                i += 1;
                continue;
            }
            _ => panic!("invalid L4TBOOT_ENTRY"),
        };
        v = v * 16 + d as u64;
        i += 1;
    }
    v
}

const UART_A: usize = 0x7000_6000;
const GICD: usize = 0x5004_1000;
const GICC: usize = 0x5004_2000;
const SB_AA64_RESET_LOW: usize = 0x6000_C230;
const SB_AA64_RESET_HIGH: usize = 0x6000_C234;
const PMC_PWRGATE_TOGGLE: usize = 0x7000_E430;
const PMC_REMOVE_CLAMPING: usize = 0x7000_E434;
const PMC_PWRGATE_STATUS: usize = 0x7000_E438;
const CAR_RST_CPUG_CMPLX_CLR: usize = 0x6000_6454;
const CPU_PARTITION: [u32; 4] = [14, 9, 10, 11];

// PSCI 1.0
const PSCI_VERSION: u64 = 0x8400_0000;
const PSCI_CPU_OFF: u64 = 0x8400_0002;
const PSCI_CPU_ON_32: u64 = 0x8400_0003;
const PSCI_CPU_ON_64: u64 = 0xC400_0003;
const PSCI_AFFINITY_INFO_32: u64 = 0x8400_0004;
const PSCI_AFFINITY_INFO_64: u64 = 0xC400_0004;
const PSCI_MIGRATE_INFO_TYPE: u64 = 0x8400_0006;
const PSCI_SYSTEM_OFF: u64 = 0x8400_0008;
const PSCI_SYSTEM_RESET: u64 = 0x8400_0009;
const PSCI_FEATURES: u64 = 0x8400_000A;
const NOT_SUPPORTED: u64 = -1i64 as u64;
const ALREADY_ON: u64 = -4i64 as u64;

/// Entry point and argument of each secondary core (written by CPU_ON).
static mut ENTRY: [u64; 4] = [0; 4];
static mut CONTEXT: [u64; 4] = [0; 4];
static mut ON: [bool; 4] = [true, false, false, false];

fn rd(a: usize) -> u32 {
    unsafe { read_volatile(a as *const u32) }
}
fn wr(a: usize, v: u32) {
    unsafe { write_volatile(a as *mut u32, v) }
}

fn putc(c: u8) {
    while rd(UART_A + 0x14) & (1 << 5) == 0 {}
    wr(UART_A, c as u32);
}
fn puts(s: &str) {
    for b in s.bytes() {
        if b == b'\n' {
            putc(b'\r');
        }
        putc(b);
    }
}
fn puthex(v: u64) {
    puts("0x");
    for i in (0..16).rev() {
        let d = ((v >> (i * 4)) & 0xF) as u8;
        putc(if d < 10 { b'0' + d } else { b'a' + d - 10 });
    }
}

/// All interrupts to Group 1: Linux runs non-secure.
fn gic_secure_init(core: usize) {
    if core == 0 {
        wr(GICD, 0);
        let lines = ((rd(GICD + 4) & 0x1F) + 1) * 32;
        for i in (0..lines).step_by(32) {
            wr(GICD + 0x80 + (i as usize / 32) * 4, 0xFFFF_FFFF);
        }
        wr(GICD, 0b11);
    }
    wr(GICD + 0x80, 0xFFFF_FFFF);
    wr(GICC, (1 << 0) | (1 << 1) | (1 << 3));
    wr(GICC + 4, 0xFF);
}

fn core_id() -> usize {
    let m: u64;
    unsafe { core::arch::asm!("mrs {}, mpidr_el1", out(reg) m) };
    (m & 0xFF) as usize
}

/// Core 0: init then jump into Linux. Secondary cores: jump to the entry
/// point given by CPU_ON.
#[no_mangle]
extern "C" fn boot_main(core: usize) -> ! {
    gic_secure_init(core);
    let (entry, arg) = if core == 0 {
        // The monitor (2 MiB at 0xFFE00000) declares itself a secure zone.
        wr(MC_SECURITY_CFG0, 0xFFE0_0000);
        wr(MC_SECURITY_CFG1, 2);
        if ENTRY_EL2 {
            puts("\n[l4tboot] EL3 monitor: non-secure GIC, PSCI 1.0; jumping to payload (EL2)\n");
        } else {
            puts("\n[l4tboot] EL3 monitor: non-secure GIC, PSCI 1.0; jumping to Linux (EL1)\n");
        }
        (KERNEL_ENTRY, DTB_ADDR)
    } else {
        unsafe { (read_volatile(&raw const ENTRY[core]), read_volatile(&raw const CONTEXT[core])) }
    };
    // EL2h or EL1h, interrupts masked.
    let spsr = if ENTRY_EL2 { 0x3C9 } else { 0x3C5 };
    unsafe { enter_el2(entry, arg, spsr) }
}

extern "C" {
    fn enter_el2(entry: u64, x0: u64, spsr: u64) -> !;
}

fn power_on(core: usize) {
    unsafe extern "C" {
        fn _start();
    }
    let v = _start as *const () as u64;
    wr(SB_AA64_RESET_LOW, (v as u32) | 1);
    wr(SB_AA64_RESET_HIGH, (v >> 32) as u32);
    let part = CPU_PARTITION[core];
    if rd(PMC_PWRGATE_STATUS) & (1 << part) == 0 {
        wr(PMC_PWRGATE_TOGGLE, part | (1 << 8));
        let mut n = 0;
        while rd(PMC_PWRGATE_STATUS) & (1 << part) == 0 && n < 100_000 {
            n += 1;
        }
    }
    wr(PMC_REMOVE_CLAMPING, 1 << part);
    let mask = (0x1111u32 << core) | (1 << (16 + core)) | (1 << (20 + core));
    wr(CAR_RST_CPUG_CMPLX_CLR, mask);
}

/// Handler for `smc` from EL2/EL1: x0 = function, x1-x3 = args.
#[no_mangle]
extern "C" fn smc_handler(regs: &mut [u64; 31]) {
    let (f, a1, a2, a3) = (regs[0], regs[1], regs[2], regs[3]);
    regs[0] = match f {
        PSCI_VERSION => 0x0001_0000,
        PSCI_MIGRATE_INFO_TYPE => 2,
        PSCI_FEATURES => match a1 {
            PSCI_VERSION | PSCI_CPU_ON_32 | PSCI_CPU_ON_64 | PSCI_CPU_OFF | PSCI_AFFINITY_INFO_32
            | PSCI_AFFINITY_INFO_64 | PSCI_SYSTEM_OFF | PSCI_SYSTEM_RESET | PSCI_FEATURES
            | PSCI_MIGRATE_INFO_TYPE => 0,
            _ => NOT_SUPPORTED,
        },
        PSCI_CPU_ON_32 | PSCI_CPU_ON_64 => {
            let c = (a1 & 0xFF) as usize;
            if c >= 4 {
                -2i64 as u64 // INVALID_PARAMETERS
            } else if unsafe { read_volatile(&raw const ON[c]) } {
                ALREADY_ON
            } else {
                unsafe {
                    write_volatile(&raw mut ENTRY[c], a2);
                    write_volatile(&raw mut CONTEXT[c], a3);
                    write_volatile(&raw mut ON[c], true);
                    core::arch::asm!("dsb sy");
                }
                power_on(c);
                0
            }
        }
        PSCI_AFFINITY_INFO_32 | PSCI_AFFINITY_INFO_64 => {
            let c = (a1 & 0xFF) as usize;
            if c < 4 && unsafe { read_volatile(&raw const ON[c]) } { 0 } else { 1 }
        }
        PSCI_CPU_OFF => {
            let c = core_id();
            unsafe { write_volatile(&raw mut ON[c], false) };
            loop {
                unsafe { core::arch::asm!("wfi") };
            }
        }
        PSCI_SYSTEM_OFF | PSCI_SYSTEM_RESET => {
            puts("\n[l4tboot] shutdown requested by Linux\n");
            loop {
                unsafe { core::arch::asm!("wfi") };
            }
        }
        _ => NOT_SUPPORTED,
    };
    let _ = a3;
}

/// Any other EL3-taken exception is a bug: print it.
#[no_mangle]
extern "C" fn el3_unexpected(regs: &mut [u64; 31]) {
    let esr: u64;
    let elr: u64;
    unsafe {
        core::arch::asm!("mrs {}, esr_el3", out(reg) esr);
        core::arch::asm!("mrs {}, elr_el3", out(reg) elr);
    }
    puts("\n[l4tboot] unexpected EL3 exception, ESR=");
    puthex(esr);
    puts(" ELR=");
    puthex(elr);
    puts("\n");
    // Trapped system-register access (EC 0x18): skip over it.
    if esr >> 26 == 0x18 {
        unsafe { core::arch::asm!("msr elr_el3, {}", in(reg) elr + 4) };
        regs[0] = regs[0];
        return;
    }
    loop {
        unsafe { core::arch::asm!("wfi") };
    }
}

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    puts("\n[l4tboot] panic\n");
    loop {}
}

global_asm!(
    r#"
.section .text.boot, "ax"
.globl _start
_start:
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
    cbnz    x19, 1f
    adrp    x0, __bss_start
    add     x0, x0, :lo12:__bss_start
    adrp    x1, __bss_end
    add     x1, x1, :lo12:__bss_end
0:  cmp     x0, x1
    b.hs    1f
    str     xzr, [x0], #8
    b       0b
1:  mov     x0, x19
    bl      boot_main

// void enter_el2(u64 entry, u64 x0, u64 spsr) -> !
.globl enter_el2
enter_el2:
    mov     x20, x0
    mov     x21, x1
    mov     x22, x2
    // SCR_EL3: NS | RES1 | HCE | RW (SMD=0: smc allowed for PSCI)
    mov     x2, #0x531
    msr     scr_el3, x2
    msr     cptr_el3, xzr
    msr     mdcr_el3, xzr
    mov     x2, #0x73                // ACTLR: IMPDEF registers accessible
    msr     actlr_el3, x2
    msr     actlr_el2, x2
    msr     cntvoff_el2, xzr
    movz    x2, #0x0830
    movk    x2, #0x30C5, lsl #16      // SCTLR_EL2: MMU and caches off
    msr     sctlr_el2, x2
    // Entry into EL1: Linux then uses the simple GIC EOI mode (single
    // EOIR write). In EL2 it would use separate EOI (GICC_DIR).
    mov     x2, #1
    lsl     x2, x2, #31              // HCR_EL2.RW: EL1 in AArch64
    msr     hcr_el2, x2
    mov     x2, #3                   // CNTHCTL_EL2: physical timer visible in EL1
    msr     cnthctl_el2, x2
    mrs     x2, midr_el1
    msr     vpidr_el2, x2
    mrs     x2, mpidr_el1
    msr     vmpidr_el2, x2
    mov     x2, #0x33FF
    msr     cptr_el2, x2
    msr     sctlr_el1, xzr
    msr     spsr_el3, x22                // EL2h or EL1h, interrupts masked
    msr     elr_el3, x20
    mov     x0, x21
    mov     x1, xzr
    mov     x2, xzr
    mov     x3, xzr
    isb
    eret

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
    cmp     x9, #0x17                // SMC from AArch64
    b.ne    2f
    bl      smc_handler
    b       3f
2:  bl      el3_unexpected
3:  RESTORE
    eret

.Lbad:
    SAVE
    mov     x0, sp
    bl      el3_unexpected
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
