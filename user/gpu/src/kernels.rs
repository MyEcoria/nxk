//! Built-in compute kernels, hand-scheduled SM 5.3 code. Threads work in
//! blocks of `BLOCK`; constant buffer 0 holds the arguments (`CB_*`).

extern crate alloc;

use alloc::vec::Vec;

use crate::sass::*;

pub const BLOCK: u32 = 128;
const BLOCK_SHIFT: i32 = 7;
/// Constant buffer 0: x and y addresses, element count, scalar, loops.
pub const CB_X: u32 = 0x00;
pub const CB_Y: u32 = 0x08;
pub const CB_COUNT: u32 = 0x10;
pub const CB_VALUE: u32 = 0x14;
pub const CB_LOOPS: u32 = 0x18;
pub const CB_SIZE: u32 = 0x100;
/// Registers any kernel here uses, rounded up.
pub const REGS: u32 = 16;
/// FFMA per thread and loop in `burn`.
pub const BURN_FFMA: u64 = 32;

/// Scoreboards: S2R results, loads.
const SB_TID: u8 = 0;
const SB_CTA: u8 = 1;
const SB_X: u8 = 2;
const SB_Y: u8 = 3;

/// Fixed-latency ALU result read by the next instruction.
fn alu() -> Sched {
    Sched::stall(6)
}

/// R0 = global thread index; threads past `CB_COUNT` exit.
fn index(a: &mut Asm) {
    a.emit(s2r(0, SR_TID_X), Sched::stall(1).wr(SB_TID).wait(0x3F));
    a.emit(s2r(1, SR_CTAID_X), Sched::stall(1).wr(SB_CTA));
    a.emit(
        shl(1, 1, BLOCK_SHIFT),
        alu().wait(1 << SB_TID | 1 << SB_CTA),
    );
    a.emit(iadd(0, 0, 1), alu());
    a.emit(isetp_c(0, 0, CB_COUNT, Cmp::Ge, false), Sched::stall(13));
    a.emit(when(exit(), 0, false), Sched::stall(15));
    a.emit(shl(2, 0, 2), alu());
}

/// Registers `d`, `d + 1` = the 64-bit address at `CB + off` plus R2.
fn address(a: &mut Asm, d: u8, off: u32) {
    a.emit(iadd_c(d, 2, off, true, false), alu());
    a.emit(iadd_c(d + 1, RZ, off + 4, false, true), alu());
}

/// x[i] = value.
pub fn fill() -> Vec<u64> {
    let mut a = Asm::new();
    index(&mut a);
    address(&mut a, 4, CB_X);
    a.emit(mov_c(3, CB_VALUE), alu());
    a.emit(stg(4, 3), Sched::stall(1));
    a.emit(exit(), Sched::stall(15));
    a.finish()
}

/// y[i] = x[i].
pub fn copy() -> Vec<u64> {
    let mut a = Asm::new();
    index(&mut a);
    address(&mut a, 4, CB_X);
    address(&mut a, 6, CB_Y);
    a.emit(ldg(3, 4), Sched::stall(1).wr(SB_X));
    a.emit(stg(6, 3), Sched::stall(1).wait(1 << SB_X));
    a.emit(exit(), Sched::stall(15));
    a.finish()
}

/// y[i] = value * x[i] + y[i].
pub fn saxpy() -> Vec<u64> {
    let mut a = Asm::new();
    index(&mut a);
    address(&mut a, 4, CB_X);
    address(&mut a, 6, CB_Y);
    a.emit(ldg(3, 4), Sched::stall(1).wr(SB_X));
    a.emit(ldg(8, 6), Sched::stall(1).wr(SB_Y));
    a.emit(ffma_c(8, 3, CB_VALUE, 8), alu().wait(1 << SB_X | 1 << SB_Y));
    a.emit(stg(6, 8), Sched::stall(1));
    a.emit(exit(), Sched::stall(15));
    a.finish()
}

/// `CB_LOOPS` times `BURN_FFMA` FFMA per thread, on eight independent
/// chains (x = x * 0.9999 + 0.0001 stays near 1): FFMA throughput.
pub fn burn() -> Vec<u64> {
    let mut a = Asm::new();
    for r in 0..8 {
        a.emit(
            mov32i(r, 0x3F80_0000),
            Sched::stall(1).wait(if r == 0 { 0x3F } else { 0 }),
        );
    }
    a.emit(mov32i(8, 0x3F7F_F972), Sched::stall(1));
    a.emit(mov32i(9, 0x38D1_B717), Sched::stall(1));
    a.emit(mov_c(10, CB_LOOPS), alu());
    let top = a.label();
    for k in 0..BURN_FFMA as u8 {
        // Each chain comes back every 8 issues, past the 6-cycle latency.
        a.emit(ffma(k % 8, k % 8, 8, 9), Sched::stall(1));
    }
    a.emit(iadd32i(10, 10, -1), alu());
    a.emit(isetp_r(0, 10, RZ, Cmp::Ne, false), Sched::stall(13));
    a.bra(top, 0, false, Sched::stall(15));
    a.emit(exit(), Sched::stall(15));
    a.finish()
}
