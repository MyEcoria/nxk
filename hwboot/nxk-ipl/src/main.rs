//! nxk-ipl: nxk's own RCM payload, run by the BPMP (ARM7TDMI) from IRAM.
//! It does what hekate does before its L4T launch (bdk `hw_init`, DRAM,
//! display, SD card), then starts nxk-bl31 on the CPU cluster. Ported
//! from hekate's bdk (GPL-2.0), checked against hekate's own run in the
//! emulator (`hwboot/oracle.sh`).

#![no_std]
#![no_main]

mod clock;
mod hdr;
mod hw;
mod i2c;
mod pmic;
mod reg;
mod sdram;
mod soc;
mod timer;
mod uart;

use core::arch::global_asm;

global_asm!(
    r#"
.section .text._start, "ax"
.arm
.global _start
_start:
    // RCM leaves the payload at 0x40010000: move it to its link address
    // with a relocator copied out of the way (hekate start.S).
    adr     r0, _start
    ldr     r1, =__ipl_start
    cmp     r0, r1
    beq     _real_start
    adr     r2, _reloc
    ldr     r3, =0x4003FF00
    mov     r4, #(_real_start - _reloc)
1:  ldmia   r2!, {{r5}}
    stmia   r3!, {{r5}}
    subs    r4, r4, #4
    bne     1b
    ldr     r2, =__ipl_end
    sub     r2, r2, r1
    ldr     r3, =_real_start
    ldr     r4, =0x4003FF00
    bx      r4
_reloc:
    ldmia   r0!, {{r4-r7}}
    stmia   r1!, {{r4-r7}}
    subs    r2, r2, #0x10
    bne     _reloc
    bx      r3
_real_start:
    ldr     sp, =0x4003FF00
    ldr     r0, =__bss_start
    ldr     r1, =__bss_end
    mov     r2, #0
2:  cmp     r0, r1
    strlo   r2, [r0], #4
    blo     2b
    ldr     r0, =ipl_main
    bx      r0
.pool
"#
);

#[no_mangle]
extern "C" fn ipl_main() -> ! {
    hw::init();
    uart::init_debug();
    uart::puts("\n[nxk-ipl] hardware up\n");
    halt()
}

/// The BPMP stops for good (hekate `bpmp_halt`).
pub fn halt() -> ! {
    loop {
        reg::wr(reg::FLOW_CTLR + 0x4, timer::HALT_MODE_WAITEVENT | timer::HALT_JTAG);
    }
}

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    uart::puts("\n[nxk-ipl] panic\n");
    halt()
}
