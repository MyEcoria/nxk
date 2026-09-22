//! Low-level boot: EL3 -> EL2 -> EL1, per-core stacks.
//! Core 0 zeroes `.bss`; the EL2 crossing opens the timer to EL1.

use core::arch::global_asm;

global_asm!(
    r#"
.section .text.boot, "ax"
.globl _start
.type _start, @function

// SCR_EL3  = NS | RES1(4,5) | SMD | HCE | RW
.equ SCR_EL3_VAL,    0x5B1
// SPSR: masked DAIF + target mode
.equ SPSR_EL2H,      0x3C9
.equ SPSR_EL1H,      0x3C5
// SCTLR_EL1: ARMv8.0 RES1 bits, MMU/caches off, unaligned access allowed
.equ SCTLR_EL1_RES1, 0x30D00800
.equ STACK_SIZE,     0x4000

_start:
    mov     x21, x0                      // x21 = optional DTB (arm64 protocol)
    mrs     x19, mpidr_el1
    and     x19, x19, #0xFF              // x19 = core number (Aff0)
    mrs     x20, CurrentEL
    lsr     x20, x20, #2                 // x20 = entry level (3: no monitor)

    // ---- core stack, set up immediately: secure code needs it ----
    adrp    x0, __stack_top
    add     x0, x0, :lo12:__stack_top
    mov     x1, #STACK_SIZE
    msub    x0, x1, x19, x0              // sp = __stack_top - core_id * STACK_SIZE
    mov     sp, x0

    // ---- where are we? ----
    mrs     x0, CurrentEL
    lsr     x0, x0, #2
    cmp     x0, #3
    b.eq    .Lel3
    cmp     x0, #2
    b.eq    .Lel2
    b       .Lel1                        // already in EL1

.Lel3:
    // Spectre v2: on Cortex-A57, IC IALLU only flushes the branch predictor
    // if CPUACTLR_EL1[0] is set, which only EL3 can do.
    mrs     x0, S3_1_C15_C2_0            // CPUACTLR_EL1
    orr     x0, x0, #1
    msr     S3_1_C15_C2_0, x0
    // CPUECTLR_EL1.SMPEN: Cortex-A57 TRM requires it before enabling
    // caches and MMU (broadcast maintenance, lock exclusives).
    mrs     x0, S3_1_C15_C2_1            // CPUECTLR_EL1
    orr     x0, x0, #(1 << 6)
    msr     S3_1_C15_C2_1, x0
    // System counter frequency: writable in EL3 only, UNKNOWN at SoC reset
    // (19.2 MHz = CLK_M, SYSCTR0_CNTFID0).
    movz    x0, #0xF800
    movk    x0, #0x0124, lsl #16
    msr     cntfrq_el0, x0
    isb

    // The GIC starts with all lines in Group 0 (secure world): without this
    // move to Group 1, non-secure EL1 would never see an IRQ.
    mov     x0, x19
    bl      nxk_secure_gic_init

    mov     x0, #SCR_EL3_VAL
    msr     scr_el3, x0
    msr     cptr_el3, xzr                // no FP/SVE traps
    msr     mdcr_el3, xzr
    mov     x0, #SPSR_EL2H
    msr     spsr_el3, x0
    adr     x0, .Lel2
    msr     elr_el3, x0
    isb
    eret

.Lel2:
    // EL2 resets UNKNOWN on SoC (zeroed by emulator): enter clean with no
    // MMU/caches/traps, PMU and debug left to EL1.
    movz    x0, #0x0830
    movk    x0, #0x30C5, lsl #16         // SCTLR_EL2: RES1 bits only
    msr     sctlr_el2, x0
    mrs     x0, pmcr_el0
    ubfx    x0, x0, #11, #5
    msr     mdcr_el2, x0
    mov     x0, #1
    lsl     x0, x0, #31                  // HCR_EL2.RW: EL1 in AArch64
    msr     hcr_el2, x0
    mov     x0, #3                       // EL1PCTEN | EL1PCEN: physical timer visible in EL1
    msr     cnthctl_el2, x0
    msr     cntvoff_el2, xzr             // no virtual timer offset
    mov     x0, #0x33FF                  // CPTR_EL2: no traps
    msr     cptr_el2, x0
    mrs     x0, midr_el1
    msr     vpidr_el2, x0
    mrs     x0, mpidr_el1
    msr     vmpidr_el2, x0
    movz    x0, #0x0800
    movk    x0, #0x30D0, lsl #16
    msr     sctlr_el1, x0                // known state before the jump
    mov     x0, #SPSR_EL1H
    msr     spsr_el2, x0
    adr     x0, .Lel1
    msr     elr_el2, x0
    isb
    eret

.Lel1:
    mov     x0, #(3 << 20)               // CPACR_EL1.FPEN: FP/SIMD allowed (for user space)
    msr     cpacr_el1, x0
    isb

    // Each exception level has its own SP in ELxh mode: the EL3 stack is
    // not the EL1 one, so set it again here.
    adrp    x0, __stack_top
    add     x0, x0, :lo12:__stack_top
    mov     x1, #STACK_SIZE
    msub    x0, x1, x19, x0              // sp = __stack_top - core_id * STACK_SIZE
    mov     sp, x0

    cbnz    x19, .Lsecondary

    // ---- core 0: zero .bss ----
    adrp    x0, __bss_start
    add     x0, x0, :lo12:__bss_start
    adrp    x1, __bss_end
    add     x1, x1, :lo12:__bss_end
.Lbss_loop:
    cmp     x0, x1
    b.hs    .Lbss_done
    stp     xzr, xzr, [x0], #16
    b       .Lbss_loop
.Lbss_done:
    mov     x0, x19
    mov     x1, x20
    mov     x2, x21
    bl      kmain
    b       .Lpark

.Lsecondary:
    mov     x0, x19
    bl      ksecondary

.Lpark:
    msr     daifset, #0xF
.Lpark_loop:
    wfe
    b       .Lpark_loop

.size _start, . - _start
"#
);
