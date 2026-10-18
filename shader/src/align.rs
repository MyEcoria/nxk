//! What the lowering can know about U32 values: how many low bits are
//! zero (a multiple of 2^k), so that a vector access needs no run-time
//! alignment test when its index is provably aligned.

use alloc::vec;
use alloc::vec::Vec;

use crate::ir::*;

fn dsts(i: &Inst, f: &mut impl FnMut(Reg)) {
    match i {
        Inst::Const { dst, .. }
        | Inst::Mov { dst, .. }
        | Inst::Un { dst, .. }
        | Inst::Bin { dst, .. }
        | Inst::Fma { dst, .. }
        | Inst::Sel { dst, .. }
        | Inst::Cmp { dst, .. }
        | Inst::Input { dst, .. }
        | Inst::Builtin { dst, .. }
        | Inst::Uniform { dst, .. }
        | Inst::UniformIdx { dst, .. }
        | Inst::BufAtomicAdd { dst, .. }
        | Inst::LocalLoad { dst, .. } => f(*dst),
        Inst::BufLoad { dsts, .. } => dsts.iter().for_each(|d| f(*d)),
        Inst::Tex { dsts, .. } => dsts.iter().for_each(|d| f(*d)),
        _ => {}
    }
}

/// For each register, a lower bound of the trailing zero bits of its value
/// (0 when nothing is known). Only registers written once get more: one
/// that is written again (a loop's counter, a variable) can be anything.
pub fn trailing_zeros(p: &Program) -> Vec<u8> {
    let n = p.reg_types.len();
    let mut defs = vec![0u32; n];
    walk(&p.body, &mut |i| dsts(i, &mut |r| defs[r.0 as usize] += 1));
    let mut tz = vec![0u8; n];
    let mut val: Vec<Option<u32>> = vec![None; n];
    walk(&p.body, &mut |i| {
        let once = |r: &Reg| defs[r.0 as usize] == 1;
        match i {
            Inst::Const { dst, bits } if once(dst) => {
                val[dst.0 as usize] = Some(*bits);
                tz[dst.0 as usize] = bits.trailing_zeros().min(32) as u8;
            }
            Inst::Mov { dst, src } if once(dst) => tz[dst.0 as usize] = tz[src.0 as usize],
            Inst::Sel { dst, a, b, .. } if once(dst) => {
                tz[dst.0 as usize] = tz[a.0 as usize].min(tz[b.0 as usize]);
            }
            Inst::Bin { op, dst, a, b } if once(dst) => {
                let (x, y) = (tz[a.0 as usize], tz[b.0 as usize]);
                tz[dst.0 as usize] = match op {
                    BinOp::IAdd | BinOp::ISub | BinOp::IMin | BinOp::IMax => x.min(y),
                    BinOp::Or | BinOp::Xor => x.min(y),
                    BinOp::And => x.max(y),
                    BinOp::IMul => (x + y).min(32),
                    BinOp::Shl => match val[b.0 as usize] {
                        Some(k) => (x + (k & 31) as u8).min(32),
                        None => 0,
                    },
                    _ => 0,
                };
            }
            _ => {}
        }
    });
    tz
}
