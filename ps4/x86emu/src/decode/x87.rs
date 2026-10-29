//! The x87 escape opcodes D8..DF and fwait.
//!
//! Transcendentals (f2xm1, fyl2x, fptan, fpatan, fyl2xp1, fsincos, fsin, fcos), BCD
//! (fbld, fbstp), fnsave / frstor and fxsave / fxrstor stay undecoded: neither vendor
//! rounds the first ones correctly and nothing shows the game uses any of them.

use super::*;

/// An x87 operand: memory of a given type (the instruction's `dst`), or ST(i).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FSrc {
    M32,
    M64,
    M80,
    I16,
    I32,
    I64,
    St(u8),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum X87Op {
    /// ST(0) = ST(0) op src. `r` is the ModRM reg field: 0 add, 1 mul, 2 com, 3 comp,
    /// 4 st0 - src, 5 src - st0, 6 st0 / src, 7 src / st0.
    Arith(u8, FSrc),
    /// ST(i) = op on ST(0) and ST(i) (same `r` meaning, ST(i) as src), then pop if set.
    ArithTo(u8, u8, bool),
    /// Push a value.
    Load(FSrc),
    /// Store ST(0) (rounded to the destination type), then pop if set.
    Store(FSrc, bool),
    /// fisttp: truncating store and pop.
    Fisttp(FSrc),
    /// fld1, fldl2t, fldl2e, fldpi, fldlg2, fldln2, fldz (D9 E8..EE, low three bits).
    Const(u8),
    Xch(u8),
    Chs,
    Abs,
    Tst,
    Xam,
    Sqrt,
    Rndint,
    Scale,
    Xtract,
    /// fprem (false) / fprem1 (true).
    Prem(bool),
    /// fcom / fucom with ST(i) or memory, popping `pops` times (`true`: unordered).
    Com(FSrc, u8, bool),
    /// fcomi / fucomi (EFLAGS), popping once if set.
    Comi(u8, bool, bool),
    /// fcmovcc: condition (the Jcc number) and ST(i).
    Cmov(u8, u8),
    Free(u8),
    Incstp,
    Decstp,
    Init,
    Clex,
    Ldcw,
    Stcw,
    StswMem,
    StswAx,
    Stenv,
    Ldenv,
    Nop,
}

fn x87(c: &Ctx, op: X87Op, mem: Operand) -> Result<Inst, DecodeError> {
    c.done(Op::X87(op), 8, mem, NONE, 0)
}

/// Decode an x87 escape `b` (D8..DF).
pub(super) fn decode(c: &mut Ctx, b: u8) -> Result<Inst, DecodeError> {
    use FSrc::*;
    use X87Op::*;
    let m = c.modrm()?;
    let r = m.reg & 7;
    if let Operand::Mem(_) = m.rm {
        let op = match (b, r) {
            (0xd8, _) => Arith(r, M32),
            (0xdc, _) => Arith(r, M64),
            (0xda, _) => Arith(r, I32),
            (0xde, _) => Arith(r, I16),
            (0xd9, 0) => Load(M32),
            (0xd9, 2) => Store(M32, false),
            (0xd9, 3) => Store(M32, true),
            (0xd9, 4) => Ldenv,
            (0xd9, 5) => Ldcw,
            (0xd9, 6) => Stenv,
            (0xd9, 7) => Stcw,
            (0xdb, 0) => Load(I32),
            (0xdb, 1) => Fisttp(I32),
            (0xdb, 2) => Store(I32, false),
            (0xdb, 3) => Store(I32, true),
            (0xdb, 5) => Load(M80),
            (0xdb, 7) => Store(M80, true),
            (0xdd, 0) => Load(M64),
            (0xdd, 1) => Fisttp(I64),
            (0xdd, 2) => Store(M64, false),
            (0xdd, 3) => Store(M64, true),
            (0xdd, 7) => StswMem,
            (0xdf, 0) => Load(I16),
            (0xdf, 1) => Fisttp(I16),
            (0xdf, 2) => Store(I16, false),
            (0xdf, 3) => Store(I16, true),
            (0xdf, 5) => Load(I64),
            (0xdf, 7) => Store(I64, true),
            _ => return Err(DecodeError::Unsupported(b)),
        };
        // fcom / fcomp with memory are Arith 2 / 3: express them as compares.
        let op = match op {
            Arith(2, s) => Com(s, 0, false),
            Arith(3, s) => Com(s, 1, false),
            op => op,
        };
        return x87(c, op, m.rm);
    }
    let Operand::Reg(i) = m.rm else {
        unreachable!()
    };
    let i = i & 7;
    let op = match (b, r) {
        (0xd8, 2) => Com(St(i), 0, false),
        (0xd8, 3) => Com(St(i), 1, false),
        (0xd8, _) => Arith(r, St(i)),
        (0xd9, 0) => Load(St(i)),
        (0xd9, 1) => Xch(i),
        (0xd9, 2) if i == 0 => Nop,
        (0xd9, 4) => match i {
            0 => Chs,
            1 => Abs,
            4 => Tst,
            5 => Xam,
            _ => return Err(DecodeError::Unsupported(b)),
        },
        (0xd9, 5) if i != 7 => Const(i),
        (0xd9, 6) => match i {
            4 => Xtract,
            5 => Prem(true),
            6 => Decstp,
            7 => Incstp,
            _ => return Err(DecodeError::Unsupported(b)),
        },
        (0xd9, 7) => match i {
            0 => Prem(false),
            2 => Sqrt,
            4 => Rndint,
            5 => Scale,
            _ => return Err(DecodeError::Unsupported(b)),
        },
        (0xda, 0..=3) => Cmov([2, 4, 6, 10][r as usize], i),
        (0xda, 5) if i == 1 => Com(St(1), 2, true),
        (0xdb, 0..=3) => Cmov([3, 5, 7, 11][r as usize], i),
        (0xdb, 4) if i == 2 => Clex,
        (0xdb, 4) if i == 3 => Init,
        (0xdb, 5) => Comi(i, false, true),
        (0xdb, 6) => Comi(i, false, false),
        (0xdc, 2 | 3) => return Err(DecodeError::Unsupported(b)),
        (0xdc, _) => ArithTo(r, i, false),
        (0xdd, 0) => Free(i),
        (0xdd, 2) => Store(St(i), false),
        (0xdd, 3) => Store(St(i), true),
        (0xdd, 4) => Com(St(i), 0, true),
        (0xdd, 5) => Com(St(i), 1, true),
        (0xde, 3) if i == 1 => Com(St(1), 2, false),
        (0xde, 2 | 3) => return Err(DecodeError::Unsupported(b)),
        (0xde, _) => ArithTo(r, i, true),
        (0xdf, 4) if i == 0 => StswAx,
        (0xdf, 5) => Comi(i, true, true),
        (0xdf, 6) => Comi(i, true, false),
        _ => return Err(DecodeError::Unsupported(b)),
    };
    x87(c, op, NONE)
}
