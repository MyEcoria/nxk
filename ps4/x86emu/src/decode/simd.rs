//! SIMD opcodes of the 0F, 0F38 and 0F3A maps, legacy and VEX encoded, plus the
//! VEX-encoded BMI1 and the 0F38 GPR instructions (MOVBE, CRC32).
//!
//! Only what Jaguar has: SSE2..SSE4.2 and AVX1. VEX.256 is accepted for the moves, the
//! ps/pd logic and vinsertf128/vextractf128; 256-bit integer forms are AVX2 and refused.

use super::*;

/// Mandatory prefix: 0 none, 1 66, 2 F3, 3 F2 (F2/F3 win over 66, as for crc32 r16).
fn pp(c: &Ctx) -> u8 {
    match c.rep {
        Rep::Rep => 2,
        Rep::Repne => 3,
        Rep::None => c.opsz as u8,
    }
}

fn is_vex(c: &Ctx) -> bool {
    c.vex & VEX != 0
}

fn wide(c: &Ctx) -> bool {
    c.vex & VEX_L != 0
}

/// Shape of an xmm instruction's operands.
#[derive(Clone, Copy, PartialEq)]
enum Form {
    /// `reg <- op(rm)`.
    Load,
    /// `rm <- reg`.
    Store,
    /// Like `Load`, plus an 8-bit immediate.
    LoadImm,
}

/// Decode the ModRM of an xmm instruction (`allow256`: the op exists as VEX.256 in AVX1).
fn xmm_op(c: &mut Ctx, op: SseOp, form: Form, allow256: bool) -> Result<Inst, DecodeError> {
    if wide(c) && !allow256 {
        return Err(DecodeError::Unsupported(0xc4));
    }
    // A VEX op without a second source must leave vvvv at 1111b (else #UD).
    if is_vex(c) && c.vvvv != 0 && one_source(op) {
        return Err(DecodeError::Unsupported(0xc4));
    }
    let m = c.modrm()?;
    let imm = if form == Form::LoadImm {
        c.r.imm(1)? & 0xff
    } else {
        0
    };
    // A mandatory prefix is not a rep for the executor.
    c.rep = Rep::None;
    let (reg, rm) = (Operand::Xmm(m.reg), xmm(m.rm));
    let size = if wide(c) { 32 } else { 16 };
    match form {
        Form::Store => c.done(Op::Sse(op), size, rm, reg, imm),
        _ => c.done(Op::Sse(op), size, reg, rm, imm),
    }
}

/// #UD for a VEX encoding naming a vvvv register the instruction has no use for.
fn no_vvvv(c: &Ctx) -> Result<(), DecodeError> {
    if is_vex(c) && c.vvvv != 0 {
        return Err(DecodeError::Unsupported(0xc4));
    }
    Ok(())
}

/// Ops that read only `src` (VEX.vvvv unused).
fn one_source(op: SseOp) -> bool {
    use SseOp::*;
    matches!(
        op,
        Mov | MovU
            | MovQ
            | MovDup
            | Abs(_)
            | Movx(..)
            | Shufd
            | Shuflw
            | Shufhw
            | Ptest
            | MovLH(LhOp::DupLo | LhOp::DupHi)
            | Fp(
                FpOp::Sqrt | FpOp::Rcp | FpOp::Rsqrt | FpOp::Round,
                Ty::Ps | Ty::Pd
            )
            | Cvt(CvtOp::Ps2Pd
                | CvtOp::Pd2Ps
                | CvtOp::Dq2Ps
                | CvtOp::Ps2Dq(_)
                | CvtOp::Dq2Pd
                | CvtOp::Pd2Dq(_))
    )
}

/// Two-source ops of the 66 0F map (pp = 1).
fn op_66_0f(b: u8) -> Option<SseOp> {
    use SseOp::*;
    Some(match b {
        0x60 => UnpackLo(1),
        0x61 => UnpackLo(2),
        0x62 => UnpackLo(4),
        0x6c => UnpackLo(8),
        0x68 => UnpackHi(1),
        0x69 => UnpackHi(2),
        0x6a => UnpackHi(4),
        0x6d => UnpackHi(8),
        0x63 => PackSS(2),
        0x6b => PackSS(4),
        0x67 => PackUS(2),
        0x64 => CmpGt(1),
        0x65 => CmpGt(2),
        0x66 => CmpGt(4),
        0x74 => CmpEq(1),
        0x75 => CmpEq(2),
        0x76 => CmpEq(4),
        0xd1 => Srl(2),
        0xd2 => Srl(4),
        0xd3 => Srl(8),
        0xe1 => Sra(2),
        0xe2 => Sra(4),
        0xf1 => Sll(2),
        0xf2 => Sll(4),
        0xf3 => Sll(8),
        0xd4 => Add(8),
        0xfc => Add(1),
        0xfd => Add(2),
        0xfe => Add(4),
        0xf8 => Sub(1),
        0xf9 => Sub(2),
        0xfa => Sub(4),
        0xfb => Sub(8),
        0xec => AddS(1),
        0xed => AddS(2),
        0xdc => AddUS(1),
        0xdd => AddUS(2),
        0xe8 => SubS(1),
        0xe9 => SubS(2),
        0xd8 => SubUS(1),
        0xd9 => SubUS(2),
        0xda => Min(1, false),
        0xde => Max(1, false),
        0xea => Min(2, true),
        0xee => Max(2, true),
        0xd5 => MulLo(2),
        0xe5 => MulHi(true),
        0xe4 => MulHi(false),
        0xf4 => MulUdq,
        0xdb => And,
        0xdf => Andn,
        0xeb => Or,
        0xef => Xor,
        _ => return None,
    })
}

/// Element type of a float op from its mandatory prefix.
fn ty(p: u8) -> Ty {
    [Ty::Ps, Ty::Pd, Ty::Ss, Ty::Sd][p as usize]
}

/// A scalar op ignores VEX.L (LIG); a packed one exists at 256 bits.
fn fp_op(c: &mut Ctx, op: SseOp, t: Ty, form: Form) -> Result<Inst, DecodeError> {
    if t.is_scalar() {
        c.vex &= !VEX_L;
    }
    xmm_op(c, op, form, !t.is_scalar())
}

/// Float opcodes of the 0F map, or `None` when `b` is not one.
fn decode_float(c: &mut Ctx, p: u8, b: u8) -> Option<Result<Inst, DecodeError>> {
    use Form::*;
    let t = ty(p);
    let arith = match b {
        0x51 => Some(FpOp::Sqrt),
        0x52 if p == 0 || p == 2 => Some(FpOp::Rsqrt),
        0x53 if p == 0 || p == 2 => Some(FpOp::Rcp),
        0x58 => Some(FpOp::Add),
        0x59 => Some(FpOp::Mul),
        0x5c => Some(FpOp::Sub),
        0x5d => Some(FpOp::Min),
        0x5e => Some(FpOp::Div),
        0x5f => Some(FpOp::Max),
        0x7c if p & 1 == 1 => Some(FpOp::HAdd),
        0x7d if p & 1 == 1 => Some(FpOp::HSub),
        0xd0 if p & 1 == 1 => Some(FpOp::AddSub),
        _ => None,
    };
    if let Some(op) = arith {
        // hadd / hsub / addsub: 66 is pd, F2 is ps.
        let t = if matches!(op, FpOp::HAdd | FpOp::HSub | FpOp::AddSub) {
            if p == 1 {
                Ty::Pd
            } else {
                Ty::Ps
            }
        } else {
            t
        };
        return Some(fp_op(c, SseOp::Fp(op, t), t, Load));
    }
    Some(match (p, b) {
        (_, 0xc2) => {
            if t.is_scalar() {
                c.vex &= !VEX_L;
            }
            let r = xmm_op(c, SseOp::Fp(FpOp::Cmp, t), LoadImm, !t.is_scalar());
            // Legacy encodings use predicates 0..8 only.
            r.map(|mut i| {
                if i.vex & VEX == 0 {
                    i.imm &= 7;
                }
                i
            })
        }
        (0 | 1, 0x2e | 0x2f) => {
            if let Err(e) = no_vvvv(c) {
                return Some(Err(e));
            }
            c.vex &= !VEX_L;
            xmm_op(c, SseOp::Comis(ty(p + 2), b == 0x2f), Load, false)
        }
        (2 | 3, 0x10 | 0x11) => {
            c.vex &= !VEX_L;
            let r = xmm_op(
                c,
                SseOp::MovS(t),
                if b == 0x10 { Load } else { Store },
                false,
            );
            // A VEX load from memory has no vvvv operand.
            match r {
                Ok(i) if i.vex & VEX != 0 && i.vvvv != 0 && matches!(i.src, Operand::Mem(_)) => {
                    Err(DecodeError::Unsupported(b))
                }
                r => r,
            }
        }
        (0 | 1, 0x12 | 0x16) => {
            let m_is_mem = c.r.b.get(c.r.pos).is_some_and(|m| m >> 6 != 3);
            let op = match (b, m_is_mem) {
                (0x12, true) => LhOp::LoadLo,
                (0x16, true) => LhOp::LoadHi,
                (0x12, false) if p == 0 => LhOp::HiToLo,
                (0x16, false) if p == 0 => LhOp::LoToHi,
                _ => return Some(Err(DecodeError::Unsupported(b))),
            };
            xmm_op(c, SseOp::MovLH(op), Load, false)
        }
        (0 | 1, 0x13 | 0x17) => {
            let op = if b == 0x13 {
                LhOp::StoreLo
            } else {
                LhOp::StoreHi
            };
            match xmm_op(c, SseOp::MovLH(op), Store, false) {
                Ok(i) if matches!(i.dst, Operand::Mem(_)) => Ok(i),
                Ok(_) => Err(DecodeError::Unsupported(b)),
                e => e,
            }
        }
        (2, 0x12) => xmm_op(c, SseOp::MovLH(LhOp::DupLo), Load, true),
        (2, 0x16) => xmm_op(c, SseOp::MovLH(LhOp::DupHi), Load, true),
        (0, 0x14) => xmm_op(c, SseOp::UnpackLo(4), Load, true),
        (0, 0x15) => xmm_op(c, SseOp::UnpackHi(4), Load, true),
        (1, 0x14) => xmm_op(c, SseOp::UnpackLo(8), Load, true),
        (1, 0x15) => xmm_op(c, SseOp::UnpackHi(8), Load, true),
        (0 | 1, 0xc6) => xmm_op(c, SseOp::Shufp(t), LoadImm, true),
        (0 | 1, 0x50) => {
            if let Err(e) = no_vvvv(c) {
                return Some(Err(e));
            }
            let m = match c.modrm() {
                Ok(m) => m,
                Err(e) => return Some(Err(e)),
            };
            let Operand::Reg(src) = m.rm else {
                return Some(Err(DecodeError::Unsupported(b)));
            };
            c.rep = Rep::None;
            c.done(
                Op::Sse(SseOp::Movmsk(t)),
                4,
                Operand::Reg(m.reg),
                Operand::Xmm(src),
                0,
            )
        }
        (_, 0x5a) => {
            let op = [CvtOp::Ps2Pd, CvtOp::Pd2Ps, CvtOp::Ss2Sd, CvtOp::Sd2Ss][p as usize];
            if p >= 2 {
                c.vex &= !VEX_L;
            }
            xmm_op(c, SseOp::Cvt(op), Load, p < 2)
        }
        (0..=2, 0x5b) => {
            let op = [CvtOp::Dq2Ps, CvtOp::Ps2Dq(false), CvtOp::Ps2Dq(true)][p as usize];
            xmm_op(c, SseOp::Cvt(op), Load, true)
        }
        (1..=3, 0xe6) => {
            let op = [CvtOp::Pd2Dq(true), CvtOp::Dq2Pd, CvtOp::Pd2Dq(false)][p as usize - 1];
            xmm_op(c, SseOp::Cvt(op), Load, true)
        }
        (2 | 3, 0x2a) => {
            c.vex &= !VEX_L;
            let size = if c.rex & 8 != 0 { 8 } else { 4 };
            let m = match c.modrm() {
                Ok(m) => m,
                Err(e) => return Some(Err(e)),
            };
            c.rep = Rep::None;
            let op = if p == 2 { CvtOp::Si2Ss } else { CvtOp::Si2Sd };
            c.done(Op::Sse(SseOp::Cvt(op)), size, Operand::Xmm(m.reg), m.rm, 0)
        }
        (2 | 3, 0x2c | 0x2d) => {
            if let Err(e) = no_vvvv(c) {
                return Some(Err(e));
            }
            c.vex &= !VEX_L;
            let size = if c.rex & 8 != 0 { 8 } else { 4 };
            let m = match c.modrm() {
                Ok(m) => m,
                Err(e) => return Some(Err(e)),
            };
            c.rep = Rep::None;
            let trunc = b == 0x2c;
            let op = if p == 2 {
                CvtOp::Ss2Si(trunc)
            } else {
                CvtOp::Sd2Si(trunc)
            };
            c.done(
                Op::Sse(SseOp::Cvt(op)),
                size,
                Operand::Reg(m.reg),
                xmm(m.rm),
                0,
            )
        }
        (0, 0xae) if is_vex(c) => {
            if let Err(e) = no_vvvv(c) {
                return Some(Err(e));
            }
            let m = match c.modrm() {
                Ok(m) => m,
                Err(e) => return Some(Err(e)),
            };
            match (m.rm, m.reg & 7) {
                (Operand::Mem(_), 2 | 3) if !wide(c) => {
                    c.done(Op::Mxcsr(m.reg & 7 == 2), 4, m.rm, NONE, 0)
                }
                _ => Err(DecodeError::Unsupported(b)),
            }
        }
        _ => return None,
    })
}

/// The 0F map's SIMD opcodes (`b` follows 0F, or a VEX prefix with map 1).
pub(super) fn decode_0f(c: &mut Ctx, b: u8) -> Result<Inst, DecodeError> {
    use Form::*;
    let p = pp(c);
    if let Some(r) = decode_float(c, p, b) {
        return r;
    }
    match (p, b) {
        (0, 0x77) if is_vex(c) => {
            let op = if wide(c) {
                SseOp::ZeroAll
            } else {
                SseOp::ZeroUpper
            };
            c.done(Op::Sse(op), 16, NONE, NONE, 0)
        }
        (0 | 1, 0x10 | 0x11) => xmm_op(c, SseOp::MovU, if b == 0x10 { Load } else { Store }, true),
        (0 | 1, 0x28 | 0x29) => xmm_op(c, SseOp::Mov, if b == 0x28 { Load } else { Store }, true),
        (0 | 1, 0x2b) | (1, 0xe7) => {
            let i = xmm_op(c, SseOp::Mov, Store, true)?;
            match i.dst {
                Operand::Mem(_) => Ok(i),
                _ => Err(DecodeError::Unsupported(b)),
            }
        }
        (0 | 1, 0x54) => xmm_op(c, SseOp::And, Load, true),
        (0 | 1, 0x55) => xmm_op(c, SseOp::Andn, Load, true),
        (0 | 1, 0x56) => xmm_op(c, SseOp::Or, Load, true),
        (0 | 1, 0x57) => xmm_op(c, SseOp::Xor, Load, true),
        (1, 0x6f | 0x7f) => xmm_op(c, SseOp::Mov, if b == 0x6f { Load } else { Store }, true),
        (2, 0x6f | 0x7f) => xmm_op(c, SseOp::MovU, if b == 0x6f { Load } else { Store }, true),
        (3, 0xf0) => {
            let i = xmm_op(c, SseOp::MovU, Load, true)?;
            match i.src {
                Operand::Mem(_) => Ok(i),
                _ => Err(DecodeError::Unsupported(b)),
            }
        }
        (2, 0x7e) => xmm_op(c, SseOp::MovQ, Load, false),
        (1, 0xd6) => xmm_op(c, SseOp::MovQ, Store, false),
        (3, 0x12) => xmm_op(c, SseOp::MovDup, Load, false),
        (1, 0x6e | 0x7e) if !wide(c) => {
            // movd/movq between a GPR (or memory) and an xmm register.
            no_vvvv(c)?;
            let m = c.modrm()?;
            let size = if c.rex & 8 != 0 { 8 } else { 4 };
            let x = Operand::Xmm(m.reg);
            if b == 0x6e {
                c.done(Op::Sse(SseOp::MovGpr), size, x, m.rm, 0)
            } else {
                c.done(Op::Sse(SseOp::MovGpr), size, m.rm, x, 0)
            }
        }
        (1, 0x70) => xmm_op(c, SseOp::Shufd, LoadImm, false),
        (3, 0x70) => xmm_op(c, SseOp::Shuflw, LoadImm, false),
        (2, 0x70) => xmm_op(c, SseOp::Shufhw, LoadImm, false),
        (1, 0x71..=0x73) if !wide(c) => {
            let m = c.modrm()?;
            let Operand::Reg(src) = m.rm else {
                return Err(DecodeError::Unsupported(b));
            };
            let imm = c.r.imm(1)? & 0xff;
            let elem = [2, 4, 8][(b - 0x71) as usize];
            let op = match (b, m.reg & 7) {
                (_, 2) => SseOp::SrlI(elem),
                (0x71 | 0x72, 4) => SseOp::SraI(elem),
                (_, 6) => SseOp::SllI(elem),
                (0x73, 3) => SseOp::Srldq,
                (0x73, 7) => SseOp::Slldq,
                _ => return Err(DecodeError::Unsupported(b)),
            };
            // Under VEX the destination is vvvv; legacy shifts the register in place.
            let dst = if is_vex(c) { c.vvvv } else { src };
            c.rep = Rep::None;
            c.done(Op::Sse(op), 16, Operand::Xmm(dst), Operand::Xmm(src), imm)
        }
        (1, 0xd7) if !wide(c) => {
            no_vvvv(c)?;
            let m = c.modrm()?;
            let Operand::Reg(src) = m.rm else {
                return Err(DecodeError::Unsupported(b));
            };
            c.done(
                Op::Sse(SseOp::Movmskb),
                4,
                Operand::Reg(m.reg),
                Operand::Xmm(src),
                0,
            )
        }
        (1, 0xc4) if !wide(c) => {
            let m = c.modrm()?;
            let imm = c.r.imm(1)? & 0xff;
            c.done(Op::Sse(SseOp::Insert(2)), 2, Operand::Xmm(m.reg), m.rm, imm)
        }
        (1, 0xc5) if !wide(c) => {
            // pextrw r32, xmm, imm: here reg is the GPR and rm the xmm register.
            no_vvvv(c)?;
            let m = c.modrm()?;
            let Operand::Reg(src) = m.rm else {
                return Err(DecodeError::Unsupported(b));
            };
            let imm = c.r.imm(1)? & 0xff;
            c.done(
                Op::Sse(SseOp::Extract(2)),
                2,
                Operand::Reg(m.reg),
                Operand::Xmm(src),
                imm,
            )
        }
        (1, _) => match op_66_0f(b) {
            Some(op) => xmm_op(c, op, Load, false),
            None => Err(DecodeError::Unsupported(b)),
        },
        _ => Err(DecodeError::Unsupported(b)),
    }
}

/// The 0F38 map: SSSE3 / SSE4.1 / SSE4.2 with 66, MOVBE and CRC32 on GPRs, VEX BMI1.
pub(super) fn decode_0f38(c: &mut Ctx, b: u8) -> Result<Inst, DecodeError> {
    use Form::*;
    use SseOp::*;
    let p = pp(c);
    let gsize = if c.rex & 8 != 0 { 8 } else { 4 };
    if is_vex(c) && p == 0 {
        if wide(c) {
            return Err(DecodeError::Unsupported(b));
        }
        let m = c.modrm()?;
        let (op, dst) = match (b, m.reg & 7) {
            (0xf2, _) => (BmiOp::Andn, m.reg),
            (0xf7, _) => (BmiOp::Bextr, m.reg),
            (0xf3, 1) => (BmiOp::Blsr, c.vvvv),
            (0xf3, 2) => (BmiOp::Blsmsk, c.vvvv),
            (0xf3, 3) => (BmiOp::Blsi, c.vvvv),
            _ => return Err(DecodeError::Unsupported(b)),
        };
        return c.done(Op::Bmi(op), gsize, Operand::Reg(dst), m.rm, 0);
    }
    if !is_vex(c) && matches!(b, 0xf0 | 0xf1) {
        let m = c.modrm()?;
        if p == 3 {
            // crc32: 32-bit accumulator (64-bit with REX.W, upper half zero), source of any size.
            let from = if b == 0xf0 {
                1
            } else if c.opsz {
                2
            } else {
                gsize
            };
            let src = if from == 1 {
                byte_reg(m.rm, c.rex)
            } else {
                m.rm
            };
            c.rep = Rep::None;
            return c.done(Op::Crc32(from), gsize, Operand::Reg(m.reg), src, 0);
        }
        if p > 1 || !matches!(m.rm, Operand::Mem(_)) {
            return Err(DecodeError::Unsupported(b));
        }
        let size = c.size();
        return if b == 0xf0 {
            c.done(Op::Movbe, size, Operand::Reg(m.reg), m.rm, 0)
        } else {
            c.done(Op::Movbe, size, m.rm, Operand::Reg(m.reg), 0)
        };
    }
    if p != 1 || wide(c) {
        return Err(DecodeError::Unsupported(b));
    }
    let (op, form) = match b {
        0x00 => (Shufb, Load),
        0x17 => (Ptest, Load),
        0x1c => (Abs(1), Load),
        0x1d => (Abs(2), Load),
        0x1e => (Abs(4), Load),
        0x20..=0x25 | 0x30..=0x35 => {
            let (from, to) = [(1, 2), (1, 4), (1, 8), (2, 4), (2, 8), (4, 8)][(b & 7) as usize];
            (Movx(from, to, b < 0x30), Load)
        }
        0x28 => (MulDq, Load),
        0x29 => (CmpEq(8), Load),
        0x2b => (PackUS(4), Load),
        0x37 => (CmpGt(8), Load),
        0x38 => (Min(1, true), Load),
        0x39 => (Min(4, true), Load),
        0x3a => (Min(2, false), Load),
        0x3b => (Min(4, false), Load),
        0x3c => (Max(1, true), Load),
        0x3d => (Max(4, true), Load),
        0x3e => (Max(2, false), Load),
        0x3f => (Max(4, false), Load),
        0x40 => (MulLo(4), Load),
        _ => return Err(DecodeError::Unsupported(b)),
    };
    xmm_op(c, op, form, false)
}

/// The 0F3A map (66 / VEX.66): palignr, pextr*, pinsr*, vinsertf128, vextractf128.
pub(super) fn decode_0f3a(c: &mut Ctx, b: u8) -> Result<Inst, DecodeError> {
    if pp(c) != 1 {
        return Err(DecodeError::Unsupported(b));
    }
    let wsize = if c.rex & 8 != 0 { 8 } else { 4 };
    match b {
        0x08..=0x0b => {
            let t = [Ty::Ps, Ty::Pd, Ty::Ss, Ty::Sd][(b - 8) as usize];
            fp_op(c, SseOp::Fp(FpOp::Round, t), t, Form::LoadImm)
        }
        0x0f => xmm_op(c, SseOp::Alignr, Form::LoadImm, false),
        0x14 | 0x15 | 0x16 | 0x20 | 0x22 if !wide(c) => {
            if b < 0x20 {
                no_vvvv(c)?;
            }
            let m = c.modrm()?;
            let imm = c.r.imm(1)? & 0xff;
            let e = match b {
                0x14 | 0x20 => 1,
                0x15 => 2,
                _ => wsize,
            };
            if b < 0x20 {
                // pextr: rm is a GPR or memory, reg the xmm source.
                c.done(
                    Op::Sse(SseOp::Extract(e)),
                    e,
                    m.rm,
                    Operand::Xmm(m.reg),
                    imm,
                )
            } else {
                c.done(Op::Sse(SseOp::Insert(e)), e, Operand::Xmm(m.reg), m.rm, imm)
            }
        }
        0x18 | 0x19 if is_vex(c) && wide(c) => {
            if b == 0x19 {
                no_vvvv(c)?;
            }
            let m = c.modrm()?;
            let imm = c.r.imm(1)? & 0xff;
            let (reg, rm) = (Operand::Xmm(m.reg), xmm(m.rm));
            if b == 0x18 {
                c.done(Op::Sse(SseOp::Insert128), 32, reg, rm, imm)
            } else {
                c.done(Op::Sse(SseOp::Extract128), 16, rm, reg, imm)
            }
        }
        _ => Err(DecodeError::Unsupported(b)),
    }
}
