//! x86-64 instruction decoder (integer subset, 32/64-bit operands).

/// Why decoding stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecodeError {
    /// The byte slice ended inside an instruction.
    Truncated,
    /// A valid x86-64 encoding this decoder does not handle yet.
    Unsupported(u8),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AluOp {
    Add,
    Or,
    Adc,
    Sbb,
    And,
    Sub,
    Xor,
    Cmp,
}

const ALU: [AluOp; 8] = [
    AluOp::Add,
    AluOp::Or,
    AluOp::Adc,
    AluOp::Sbb,
    AluOp::And,
    AluOp::Sub,
    AluOp::Xor,
    AluOp::Cmp,
];

/// Memory operand `[base + index*scale + disp]` (RIP-relative when `rip`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MemRef {
    pub base: Option<u8>,
    pub index: Option<(u8, u8)>,
    pub disp: i64,
    pub rip: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Operand {
    Reg(u8),
    Mem(MemRef),
    Imm(i64),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    Alu(AluOp),
    Test,
    Mov,
    Lea,
    Push,
    Pop,
    Ret,
    Nop,
    Jmp,
    /// Condition code 0..=15 (O, NO, B, AE, E, NE, BE, A, S, NS, P, NP, L, GE, LE, G).
    Jcc(u8),
}

/// A decoded instruction. `size` is the operand size in bytes (4 or 8).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Inst {
    pub len: u8,
    pub op: Op,
    pub size: u8,
    pub dst: Operand,
    pub src: Operand,
}

struct Reader<'a> {
    b: &'a [u8],
    pos: usize,
}

impl Reader<'_> {
    fn u8(&mut self) -> Result<u8, DecodeError> {
        let v = *self.b.get(self.pos).ok_or(DecodeError::Truncated)?;
        self.pos += 1;
        Ok(v)
    }
    fn imm(&mut self, n: usize) -> Result<i64, DecodeError> {
        let mut v = 0u64;
        for i in 0..n {
            v |= (self.u8()? as u64) << (8 * i);
        }
        let shift = 64 - 8 * n as u32;
        Ok(((v << shift) as i64) >> shift)
    }
}

/// ModRM decoded into the `reg` field and the r/m operand.
struct ModRm {
    reg: u8,
    rm: Operand,
}

fn modrm(r: &mut Reader, rex: u8) -> Result<ModRm, DecodeError> {
    let m = r.u8()?;
    let (md, reg, rm) = (m >> 6, ((m >> 3) & 7) | ((rex & 4) << 1), m & 7);
    if md == 3 {
        return Ok(ModRm {
            reg,
            rm: Operand::Reg(rm | ((rex & 1) << 3)),
        });
    }
    let mut mem = MemRef {
        base: None,
        index: None,
        disp: 0,
        rip: false,
    };
    if rm == 4 {
        let sib = r.u8()?;
        let (scale, idx, base) = (sib >> 6, ((sib >> 3) & 7) | ((rex & 2) << 2), sib & 7);
        if idx != 4 {
            mem.index = Some((idx, 1 << scale));
        }
        if base == 5 && md == 0 {
            mem.disp = r.imm(4)?;
        } else {
            mem.base = Some(base | ((rex & 1) << 3));
        }
    } else if rm == 5 && md == 0 {
        mem.rip = true;
        mem.disp = r.imm(4)?;
    } else {
        mem.base = Some(rm | ((rex & 1) << 3));
    }
    match md {
        1 => mem.disp = r.imm(1)?,
        2 => mem.disp = r.imm(4)?,
        _ => {}
    }
    Ok(ModRm {
        reg,
        rm: Operand::Mem(mem),
    })
}

/// Decode one instruction.
///
/// # Args
/// * `code` - bytes starting at the instruction
///
/// # Returns
/// The instruction, or why it could not be decoded.
///
/// # Example
/// ```
/// let i = x86emu::decode::decode(&[0x48, 0x01, 0xd8]).unwrap(); // add rax, rbx
/// assert_eq!(i.len, 3);
/// ```
pub fn decode(code: &[u8]) -> Result<Inst, DecodeError> {
    let mut r = Reader { b: code, pos: 0 };
    let mut rex = 0;
    let mut b = r.u8()?;
    if b & 0xf0 == 0x40 {
        rex = b;
        b = r.u8()?;
    }
    let size = if rex & 8 != 0 { 8 } else { 4 };
    let mk = |op, dst, src, r: &Reader, size| {
        Ok(Inst {
            len: r.pos as u8,
            op,
            size,
            dst,
            src,
        })
    };
    match b {
        0x00..=0x3f if matches!(b & 7, 1 | 3) => {
            let op = Op::Alu(ALU[(b >> 3) as usize & 7]);
            let m = modrm(&mut r, rex)?;
            if b & 2 == 0 {
                mk(op, m.rm, Operand::Reg(m.reg), &r, size)
            } else {
                mk(op, Operand::Reg(m.reg), m.rm, &r, size)
            }
        }
        0x81 | 0x83 => {
            let m = modrm(&mut r, rex)?;
            let imm = r.imm(if b == 0x81 { 4 } else { 1 })?;
            mk(
                Op::Alu(ALU[(m.reg & 7) as usize]),
                m.rm,
                Operand::Imm(imm),
                &r,
                size,
            )
        }
        0x85 => {
            let m = modrm(&mut r, rex)?;
            mk(Op::Test, m.rm, Operand::Reg(m.reg), &r, size)
        }
        0x89 | 0x8b => {
            let m = modrm(&mut r, rex)?;
            if b == 0x89 {
                mk(Op::Mov, m.rm, Operand::Reg(m.reg), &r, size)
            } else {
                mk(Op::Mov, Operand::Reg(m.reg), m.rm, &r, size)
            }
        }
        0x8d => {
            let m = modrm(&mut r, rex)?;
            if matches!(m.rm, Operand::Reg(_)) {
                return Err(DecodeError::Unsupported(b));
            }
            mk(Op::Lea, Operand::Reg(m.reg), m.rm, &r, size)
        }
        0xb8..=0xbf => {
            let imm = r.imm(size as usize)?;
            mk(
                Op::Mov,
                Operand::Reg((b & 7) | ((rex & 1) << 3)),
                Operand::Imm(imm),
                &r,
                size,
            )
        }
        0xc7 => {
            let m = modrm(&mut r, rex)?;
            if m.reg & 7 != 0 {
                return Err(DecodeError::Unsupported(b));
            }
            let imm = r.imm(4)?;
            mk(Op::Mov, m.rm, Operand::Imm(imm), &r, size)
        }
        0x50..=0x57 => mk(
            Op::Push,
            Operand::Reg((b & 7) | ((rex & 1) << 3)),
            Operand::Imm(0),
            &r,
            8,
        ),
        0x58..=0x5f => mk(
            Op::Pop,
            Operand::Reg((b & 7) | ((rex & 1) << 3)),
            Operand::Imm(0),
            &r,
            8,
        ),
        0x90 if rex & 1 == 0 => mk(Op::Nop, Operand::Imm(0), Operand::Imm(0), &r, size),
        0xc3 => mk(Op::Ret, Operand::Imm(0), Operand::Imm(0), &r, 8),
        0xeb | 0xe9 => {
            let rel = r.imm(if b == 0xeb { 1 } else { 4 })?;
            mk(Op::Jmp, Operand::Imm(rel), Operand::Imm(0), &r, 8)
        }
        0x70..=0x7f => {
            let rel = r.imm(1)?;
            mk(Op::Jcc(b & 15), Operand::Imm(rel), Operand::Imm(0), &r, 8)
        }
        0x0f => match r.u8()? {
            c @ 0x80..=0x8f => {
                let rel = r.imm(4)?;
                mk(Op::Jcc(c & 15), Operand::Imm(rel), Operand::Imm(0), &r, 8)
            }
            c => Err(DecodeError::Unsupported(c)),
        },
        _ => Err(DecodeError::Unsupported(b)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_basics() {
        let i = decode(&[0x48, 0x01, 0xd8]).unwrap();
        assert_eq!(
            (i.op, i.dst, i.src, i.size),
            (Op::Alu(AluOp::Add), Operand::Reg(0), Operand::Reg(3), 8)
        );
        let i = decode(&[0x41, 0x8b, 0x44, 0x24, 0x08]).unwrap(); // mov eax,[r12+8]
        assert_eq!(i.len, 5);
        assert_eq!(
            i.src,
            Operand::Mem(MemRef {
                base: Some(12),
                index: None,
                disp: 8,
                rip: false
            })
        );
        let i = decode(&[0x48, 0x83, 0xec, 0x20]).unwrap(); // sub rsp,32
        assert_eq!((i.dst, i.src), (Operand::Reg(4), Operand::Imm(32)));
        let i = decode(&[0x48, 0x8d, 0x04, 0x8b]).unwrap(); // lea rax,[rbx+rcx*4]
        assert_eq!(
            i.src,
            Operand::Mem(MemRef {
                base: Some(3),
                index: Some((1, 4)),
                disp: 0,
                rip: false
            })
        );
        assert_eq!(decode(&[0x7e, 0xfe]).unwrap().op, Op::Jcc(14));
    }

    #[test]
    fn errors() {
        assert_eq!(decode(&[0x48]), Err(DecodeError::Truncated));
        assert_eq!(decode(&[0x01]), Err(DecodeError::Truncated));
        assert_eq!(decode(&[0xf4]), Err(DecodeError::Unsupported(0xf4)));
    }
}
