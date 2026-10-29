//! x86-64 instruction decoder: the general-purpose integer set, string moves and SSE2
//! integer / data moves, as compilers emit them (8/16/32/64-bit operands).

/// Why decoding stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecodeError {
    /// The byte slice ended inside an instruction.
    Truncated,
    /// A valid x86-64 encoding this decoder does not handle yet (opcode byte; 0x0f map
    /// opcodes are reported as their second byte).
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

/// Group-2 shifts and rotates, in ModRM `reg` order (6 is an alias of Shl).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShiftOp {
    Rol,
    Ror,
    Rcl,
    Rcr,
    Shl,
    Shr,
    Sar,
}

const SHIFT: [ShiftOp; 8] = [
    ShiftOp::Rol,
    ShiftOp::Ror,
    ShiftOp::Rcl,
    ShiftOp::Rcr,
    ShiftOp::Shl,
    ShiftOp::Shr,
    ShiftOp::Shl,
    ShiftOp::Sar,
];

/// One-operand group-3/4/5 operations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnOp {
    Not,
    Neg,
    Inc,
    Dec,
    /// Unsigned `rdx:rax = rax * src`.
    Mul,
    /// Signed one-operand form of `Mul`.
    Imul,
    Div,
    Idiv,
}

/// Bit-scan and bit-count operations (`dst = f(src)`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BitOp {
    Bsf,
    Bsr,
    Tzcnt,
    Lzcnt,
    Popcnt,
}

/// `bt`, `bts`, `btr`, `btc`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BtOp {
    Bt,
    Bts,
    Btr,
    Btc,
}

/// SSE2 operations on xmm registers. `u8` payloads are element sizes in bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SseOp {
    /// 128-bit move (movdqa/movdqu/movaps/movups/movapd/movupd).
    Mov,
    /// movd/movq into an xmm register (zero-extended) or out of one; `size` is 4 or 8.
    MovGpr,
    /// movq xmm, xmm/m64 (zero-extended) and movq m64, xmm.
    MovQ,
    Xor,
    And,
    Andn,
    Or,
    Add(u8),
    Sub(u8),
    CmpEq(u8),
    CmpGt(u8),
    /// pmuludq.
    MulUdq,
    /// Shift each element by an immediate (`imm`) or by xmm/m128's low quadword.
    Sll(u8),
    Srl(u8),
    Sra(u8),
    /// Whole-register byte shifts (pslldq / psrldq) by `imm`.
    Slldq,
    Srldq,
    /// pshufd with `imm`.
    Shufd,
    UnpackLo(u8),
    UnpackHi(u8),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Seg {
    None,
    Fs,
    Gs,
}

/// Memory operand `seg:[base + index*scale + disp]` (RIP-relative when `rip`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MemRef {
    pub base: Option<u8>,
    pub index: Option<(u8, u8)>,
    pub disp: i64,
    pub rip: bool,
    pub seg: Seg,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Operand {
    Reg(u8),
    /// ah, ch, dh, bh (bits 8..16 of rax, rcx, rdx, rbx).
    RegH(u8),
    Mem(MemRef),
    Imm(i64),
    Xmm(u8),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    Alu(AluOp),
    Test,
    Mov,
    /// Zero / sign extension from a source of the given byte size.
    Movzx(u8),
    Movsx(u8),
    Lea,
    Xchg,
    Push,
    Pop,
    /// `ret`, popping `imm` more bytes.
    Ret,
    Call,
    Jmp,
    /// Condition code 0..=15 (O, NO, B, AE, E, NE, BE, A, S, NS, P, NP, L, GE, LE, G).
    Jcc(u8),
    Setcc(u8),
    Cmovcc(u8),
    Leave,
    Shift(ShiftOp),
    Unary(UnOp),
    /// `dst = dst * src` (two operands) or `dst = src * imm` (three, `imm` set by the form).
    Imul2,
    Imul3,
    /// cbw / cwde / cdqe: sign-extend the low half of the accumulator.
    ExtendAcc,
    /// cwd / cdq / cqo: rdx = sign of rax.
    SignAcc,
    Bswap,
    Bits(BitOp),
    Bt(BtOp),
    Cmpxchg,
    Xadd,
    /// movs / stos; `size` is the element size, `rep` the prefix.
    Movs,
    Stos,
    Cld,
    Std,
    Nop,
    Hlt,
    /// ud2 or int3: a deliberate trap.
    Trap,
    Sse(SseOp),
}

/// `rep` / `repne` prefix state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rep {
    None,
    Rep,
    Repne,
}

/// A decoded instruction. `size` is the operand size in bytes (1, 2, 4 or 8).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Inst {
    pub len: u8,
    pub op: Op,
    pub size: u8,
    pub dst: Operand,
    pub src: Operand,
    /// Third operand: `imul` / `pshufd` / shift immediates, `ret` pop count.
    pub imm: i64,
    pub rep: Rep,
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

fn modrm(r: &mut Reader, rex: u8, seg: Seg) -> Result<ModRm, DecodeError> {
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
        seg,
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

/// Register operand of a byte-sized instruction: without REX, 4..8 are ah..bh.
fn byte_reg(o: Operand, rex: u8) -> Operand {
    match o {
        Operand::Reg(r) if rex == 0 && (4..8).contains(&r) => Operand::RegH(r - 4),
        o => o,
    }
}

fn xmm(o: Operand) -> Operand {
    match o {
        Operand::Reg(r) => Operand::Xmm(r),
        o => o,
    }
}

/// Decoding context shared by the opcode tables.
struct Ctx<'a> {
    r: Reader<'a>,
    rex: u8,
    seg: Seg,
    /// 0x66 seen.
    opsz: bool,
    rep: Rep,
}

impl Ctx<'_> {
    /// Operand size of a non-byte instruction.
    fn size(&self) -> u8 {
        if self.rex & 8 != 0 {
            8
        } else if self.opsz {
            2
        } else {
            4
        }
    }
    fn modrm(&mut self) -> Result<ModRm, DecodeError> {
        modrm(&mut self.r, self.rex, self.seg)
    }
    /// Immediate of an `iz` operand: 2 bytes for 16-bit operands, else 4 (sign-extended).
    fn imm_z(&mut self, size: u8) -> Result<i64, DecodeError> {
        self.r.imm(if size == 2 { 2 } else { 4 })
    }
    fn done(
        &self,
        op: Op,
        size: u8,
        dst: Operand,
        src: Operand,
        imm: i64,
    ) -> Result<Inst, DecodeError> {
        Ok(Inst {
            len: self.r.pos as u8,
            op,
            size,
            dst,
            src,
            imm,
            rep: self.rep,
        })
    }
}

const NONE: Operand = Operand::Imm(0);

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
    let mut c = Ctx {
        r: Reader { b: code, pos: 0 },
        rex: 0,
        seg: Seg::None,
        opsz: false,
        rep: Rep::None,
    };
    let mut b = c.r.u8()?;
    loop {
        match b {
            0x66 => c.opsz = true,
            0xf2 => c.rep = Rep::Repne,
            0xf3 => c.rep = Rep::Rep,
            0x64 => c.seg = Seg::Fs,
            0x65 => c.seg = Seg::Gs,
            0xf0 | 0x2e | 0x3e | 0x26 | 0x36 => {}
            _ => break,
        }
        if c.r.pos >= 14 {
            return Err(DecodeError::Unsupported(b));
        }
        b = c.r.u8()?;
    }
    if b & 0xf0 == 0x40 {
        c.rex = b;
        b = c.r.u8()?;
    }
    if b == 0x0f {
        let b2 = c.r.u8()?;
        return decode_0f(&mut c, b2);
    }
    decode_1byte(&mut c, b)
}

fn decode_1byte(c: &mut Ctx, b: u8) -> Result<Inst, DecodeError> {
    let size = c.size();
    let rex = c.rex;
    match b {
        0x00..=0x3f if b & 7 < 6 => {
            let op = Op::Alu(ALU[(b >> 3) as usize & 7]);
            match b & 7 {
                0 | 2 => {
                    let m = c.modrm()?;
                    let (rm, reg) = (byte_reg(m.rm, rex), byte_reg(Operand::Reg(m.reg), rex));
                    if b & 2 == 0 {
                        c.done(op, 1, rm, reg, 0)
                    } else {
                        c.done(op, 1, reg, rm, 0)
                    }
                }
                1 | 3 => {
                    let m = c.modrm()?;
                    if b & 2 == 0 {
                        c.done(op, size, m.rm, Operand::Reg(m.reg), 0)
                    } else {
                        c.done(op, size, Operand::Reg(m.reg), m.rm, 0)
                    }
                }
                4 => {
                    let imm = c.r.imm(1)?;
                    c.done(op, 1, Operand::Reg(0), Operand::Imm(imm), 0)
                }
                _ => {
                    let imm = c.imm_z(size)?;
                    c.done(op, size, Operand::Reg(0), Operand::Imm(imm), 0)
                }
            }
        }
        0x50..=0x57 => c.done(
            Op::Push,
            8,
            Operand::Reg((b & 7) | ((rex & 1) << 3)),
            NONE,
            0,
        ),
        0x58..=0x5f => c.done(
            Op::Pop,
            8,
            Operand::Reg((b & 7) | ((rex & 1) << 3)),
            NONE,
            0,
        ),
        0x63 => {
            let m = c.modrm()?;
            c.done(Op::Movsx(4), size, Operand::Reg(m.reg), m.rm, 0)
        }
        0x68 | 0x6a => {
            let imm = c.r.imm(if b == 0x68 { 4 } else { 1 })?;
            c.done(Op::Push, 8, Operand::Imm(imm), NONE, 0)
        }
        0x69 | 0x6b => {
            let m = c.modrm()?;
            let imm = if b == 0x69 {
                c.imm_z(size)?
            } else {
                c.r.imm(1)?
            };
            c.done(Op::Imul3, size, Operand::Reg(m.reg), m.rm, imm)
        }
        0x70..=0x7f => {
            let rel = c.r.imm(1)?;
            c.done(Op::Jcc(b & 15), 8, Operand::Imm(rel), NONE, 0)
        }
        0x80 | 0x81 | 0x83 => {
            let m = c.modrm()?;
            let (sz, dst) = if b == 0x80 {
                (1, byte_reg(m.rm, rex))
            } else {
                (size, m.rm)
            };
            let imm = if b == 0x81 { c.imm_z(sz)? } else { c.r.imm(1)? };
            c.done(
                Op::Alu(ALU[(m.reg & 7) as usize]),
                sz,
                dst,
                Operand::Imm(imm),
                0,
            )
        }
        0x84..=0x87 => {
            let m = c.modrm()?;
            let op = if b < 0x86 { Op::Test } else { Op::Xchg };
            if b & 1 == 0 {
                c.done(
                    op,
                    1,
                    byte_reg(m.rm, rex),
                    byte_reg(Operand::Reg(m.reg), rex),
                    0,
                )
            } else {
                c.done(op, size, m.rm, Operand::Reg(m.reg), 0)
            }
        }
        0x88..=0x8b => {
            let m = c.modrm()?;
            let (sz, rm, reg) = if b & 1 == 0 {
                (1, byte_reg(m.rm, rex), byte_reg(Operand::Reg(m.reg), rex))
            } else {
                (size, m.rm, Operand::Reg(m.reg))
            };
            if b & 2 == 0 {
                c.done(Op::Mov, sz, rm, reg, 0)
            } else {
                c.done(Op::Mov, sz, reg, rm, 0)
            }
        }
        0x8d => {
            let m = c.modrm()?;
            if !matches!(m.rm, Operand::Mem(_)) {
                return Err(DecodeError::Unsupported(b));
            }
            c.done(Op::Lea, size, Operand::Reg(m.reg), m.rm, 0)
        }
        0x8f => {
            let m = c.modrm()?;
            if m.reg & 7 != 0 {
                return Err(DecodeError::Unsupported(b));
            }
            c.done(Op::Pop, 8, m.rm, NONE, 0)
        }
        0x90 if rex & 1 == 0 => c.done(Op::Nop, size, NONE, NONE, 0),
        0x90..=0x97 => c.done(
            Op::Xchg,
            size,
            Operand::Reg((b & 7) | ((rex & 1) << 3)),
            Operand::Reg(0),
            0,
        ),
        0x98 => c.done(Op::ExtendAcc, size, NONE, NONE, 0),
        0x99 => c.done(Op::SignAcc, size, NONE, NONE, 0),
        0xa4 | 0xa5 | 0xaa | 0xab => {
            let op = if b < 0xa8 { Op::Movs } else { Op::Stos };
            c.done(op, if b & 1 == 0 { 1 } else { size }, NONE, NONE, 0)
        }
        0xa8 => {
            let imm = c.r.imm(1)?;
            c.done(Op::Test, 1, Operand::Reg(0), Operand::Imm(imm), 0)
        }
        0xa9 => {
            let imm = c.imm_z(size)?;
            c.done(Op::Test, size, Operand::Reg(0), Operand::Imm(imm), 0)
        }
        0xb0..=0xb7 => {
            let imm = c.r.imm(1)?;
            let dst = byte_reg(Operand::Reg((b & 7) | ((rex & 1) << 3)), rex);
            c.done(Op::Mov, 1, dst, Operand::Imm(imm), 0)
        }
        0xb8..=0xbf => {
            let imm = c.r.imm(size as usize)?;
            c.done(
                Op::Mov,
                size,
                Operand::Reg((b & 7) | ((rex & 1) << 3)),
                Operand::Imm(imm),
                0,
            )
        }
        0xc0 | 0xc1 | 0xd0..=0xd3 => {
            let m = c.modrm()?;
            let (sz, dst) = if b & 1 == 0 {
                (1, byte_reg(m.rm, rex))
            } else {
                (size, m.rm)
            };
            let count = match b {
                0xc0 | 0xc1 => Operand::Imm(c.r.imm(1)? & 0xff),
                0xd0 | 0xd1 => Operand::Imm(1),
                _ => Operand::Reg(1),
            };
            c.done(Op::Shift(SHIFT[(m.reg & 7) as usize]), sz, dst, count, 0)
        }
        0xc2 => {
            let n = c.r.imm(2)? & 0xffff;
            c.done(Op::Ret, 8, NONE, NONE, n)
        }
        0xc3 => c.done(Op::Ret, 8, NONE, NONE, 0),
        0xc6 | 0xc7 => {
            let m = c.modrm()?;
            if m.reg & 7 != 0 {
                return Err(DecodeError::Unsupported(b));
            }
            if b == 0xc6 {
                let imm = c.r.imm(1)?;
                c.done(Op::Mov, 1, byte_reg(m.rm, rex), Operand::Imm(imm), 0)
            } else {
                let imm = c.imm_z(size)?;
                c.done(Op::Mov, size, m.rm, Operand::Imm(imm), 0)
            }
        }
        0xc9 => c.done(Op::Leave, 8, NONE, NONE, 0),
        0xcc => c.done(Op::Trap, 8, NONE, NONE, 0),
        0xe8 => {
            let rel = c.r.imm(4)?;
            c.done(Op::Call, 8, Operand::Imm(rel), NONE, 0)
        }
        0xe9 | 0xeb => {
            let rel = c.r.imm(if b == 0xeb { 1 } else { 4 })?;
            c.done(Op::Jmp, 8, Operand::Imm(rel), NONE, 0)
        }
        0xf4 => c.done(Op::Hlt, 8, NONE, NONE, 0),
        0xf6 | 0xf7 => {
            let m = c.modrm()?;
            let (sz, dst) = if b == 0xf6 {
                (1, byte_reg(m.rm, rex))
            } else {
                (size, m.rm)
            };
            let op = match m.reg & 7 {
                0 | 1 => {
                    let imm = if sz == 1 { c.r.imm(1)? } else { c.imm_z(sz)? };
                    return c.done(Op::Test, sz, dst, Operand::Imm(imm), 0);
                }
                2 => UnOp::Not,
                3 => UnOp::Neg,
                4 => UnOp::Mul,
                5 => UnOp::Imul,
                6 => UnOp::Div,
                _ => UnOp::Idiv,
            };
            c.done(Op::Unary(op), sz, dst, NONE, 0)
        }
        0xfc => c.done(Op::Cld, 8, NONE, NONE, 0),
        0xfd => c.done(Op::Std, 8, NONE, NONE, 0),
        0xfe | 0xff => {
            let m = c.modrm()?;
            let (sz, dst) = if b == 0xfe {
                (1, byte_reg(m.rm, rex))
            } else {
                (size, m.rm)
            };
            match (b, m.reg & 7) {
                (_, 0) => c.done(Op::Unary(UnOp::Inc), sz, dst, NONE, 0),
                (_, 1) => c.done(Op::Unary(UnOp::Dec), sz, dst, NONE, 0),
                (0xff, 2) => c.done(Op::Call, 8, m.rm, NONE, 0),
                (0xff, 4) => c.done(Op::Jmp, 8, m.rm, NONE, 0),
                (0xff, 6) => c.done(Op::Push, 8, m.rm, NONE, 0),
                _ => Err(DecodeError::Unsupported(b)),
            }
        }
        _ => Err(DecodeError::Unsupported(b)),
    }
}

fn decode_0f(c: &mut Ctx, b: u8) -> Result<Inst, DecodeError> {
    let size = c.size();
    let rex = c.rex;
    match b {
        0x0b => c.done(Op::Trap, 8, NONE, NONE, 0),
        // Hint nops, prefetches and fences have no architectural effect here.
        0x18 | 0x1f => {
            c.modrm()?;
            c.done(Op::Nop, size, NONE, NONE, 0)
        }
        0xae => {
            let m = c.modrm()?;
            if !matches!(m.rm, Operand::Reg(_)) || m.reg & 7 < 5 {
                return Err(DecodeError::Unsupported(b));
            }
            c.done(Op::Nop, size, NONE, NONE, 0)
        }
        0x40..=0x4f => {
            let m = c.modrm()?;
            c.done(Op::Cmovcc(b & 15), size, Operand::Reg(m.reg), m.rm, 0)
        }
        0x80..=0x8f => {
            let rel = c.r.imm(4)?;
            c.done(Op::Jcc(b & 15), 8, Operand::Imm(rel), NONE, 0)
        }
        0x90..=0x9f => {
            let m = c.modrm()?;
            c.done(Op::Setcc(b & 15), 1, byte_reg(m.rm, rex), NONE, 0)
        }
        0xa3 | 0xab | 0xb3 | 0xbb => {
            let m = c.modrm()?;
            let op = [BtOp::Bt, BtOp::Bts, BtOp::Btr, BtOp::Btc][((b >> 3) & 3) as usize];
            c.done(Op::Bt(op), size, m.rm, Operand::Reg(m.reg), 0)
        }
        0xba => {
            let m = c.modrm()?;
            let imm = c.r.imm(1)? & 0xff;
            let op = match m.reg & 7 {
                4 => BtOp::Bt,
                5 => BtOp::Bts,
                6 => BtOp::Btr,
                7 => BtOp::Btc,
                _ => return Err(DecodeError::Unsupported(b)),
            };
            c.done(Op::Bt(op), size, m.rm, Operand::Imm(imm), 0)
        }
        0xaf => {
            let m = c.modrm()?;
            c.done(Op::Imul2, size, Operand::Reg(m.reg), m.rm, 0)
        }
        0xb0 | 0xb1 | 0xc0 | 0xc1 => {
            let m = c.modrm()?;
            let op = if b < 0xc0 { Op::Cmpxchg } else { Op::Xadd };
            if b & 1 == 0 {
                c.done(
                    op,
                    1,
                    byte_reg(m.rm, rex),
                    byte_reg(Operand::Reg(m.reg), rex),
                    0,
                )
            } else {
                c.done(op, size, m.rm, Operand::Reg(m.reg), 0)
            }
        }
        0xb6 | 0xb7 | 0xbe | 0xbf => {
            let m = c.modrm()?;
            let from = if b & 1 == 0 { 1 } else { 2 };
            let src = if from == 1 { byte_reg(m.rm, rex) } else { m.rm };
            let op = if b < 0xb8 {
                Op::Movzx(from)
            } else {
                Op::Movsx(from)
            };
            c.done(op, size, Operand::Reg(m.reg), src, 0)
        }
        0xb8 if c.rep == Rep::Rep => {
            let m = c.modrm()?;
            c.done(Op::Bits(BitOp::Popcnt), size, Operand::Reg(m.reg), m.rm, 0)
        }
        0xbc | 0xbd => {
            let m = c.modrm()?;
            let op = match (b, c.rep == Rep::Rep) {
                (0xbc, false) => BitOp::Bsf,
                (0xbc, true) => BitOp::Tzcnt,
                (_, false) => BitOp::Bsr,
                (_, true) => BitOp::Lzcnt,
            };
            c.done(Op::Bits(op), size, Operand::Reg(m.reg), m.rm, 0)
        }
        0xc8..=0xcf => c.done(
            Op::Bswap,
            size,
            Operand::Reg((b & 7) | ((rex & 1) << 3)),
            NONE,
            0,
        ),
        _ => decode_sse(c, b),
    }
}

/// SSE2 integer and data-move opcodes of the 0x0f map.
fn decode_sse(c: &mut Ctx, b: u8) -> Result<Inst, DecodeError> {
    let (p66, f3) = (c.opsz, c.rep == Rep::Rep);
    // A mandatory prefix is not a rep for the executor.
    let sse = |c: &mut Ctx, op: SseOp, load: bool, imm: bool| -> Result<Inst, DecodeError> {
        let m = c.modrm()?;
        let imm = if imm { c.r.imm(1)? & 0xff } else { 0 };
        c.rep = Rep::None;
        let (reg, rm) = (Operand::Xmm(m.reg), xmm(m.rm));
        if load {
            c.done(Op::Sse(op), 16, reg, rm, imm)
        } else {
            c.done(Op::Sse(op), 16, rm, reg, imm)
        }
    };
    match b {
        0x10 | 0x11 | 0x28 | 0x29 if !f3 && c.rep == Rep::None => {
            sse(c, SseOp::Mov, b & 1 == 0, false)
        }
        0x6f | 0x7f if p66 || f3 => sse(c, SseOp::Mov, b == 0x6f, false),
        0x54 if !f3 => sse(c, SseOp::And, true, false),
        0x55 if !f3 => sse(c, SseOp::Andn, true, false),
        0x56 if !f3 => sse(c, SseOp::Or, true, false),
        0x57 if !f3 => sse(c, SseOp::Xor, true, false),
        0x6e | 0x7e if p66 => {
            // movd/movq between a GPR (or memory) and an xmm register.
            let m = c.modrm()?;
            let size = if c.rex & 8 != 0 { 8 } else { 4 };
            let (x, g) = (Operand::Xmm(m.reg), m.rm);
            if b == 0x6e {
                c.done(Op::Sse(SseOp::MovGpr), size, x, g, 0)
            } else {
                c.done(Op::Sse(SseOp::MovGpr), size, g, x, 0)
            }
        }
        0x7e if f3 => sse(c, SseOp::MovQ, true, false),
        0xd6 if p66 => sse(c, SseOp::MovQ, false, false),
        _ if !p66 => Err(DecodeError::Unsupported(b)),
        0x60 => sse(c, SseOp::UnpackLo(1), true, false),
        0x61 => sse(c, SseOp::UnpackLo(2), true, false),
        0x62 => sse(c, SseOp::UnpackLo(4), true, false),
        0x6c => sse(c, SseOp::UnpackLo(8), true, false),
        0x68 => sse(c, SseOp::UnpackHi(1), true, false),
        0x69 => sse(c, SseOp::UnpackHi(2), true, false),
        0x6a => sse(c, SseOp::UnpackHi(4), true, false),
        0x6d => sse(c, SseOp::UnpackHi(8), true, false),
        0x64 => sse(c, SseOp::CmpGt(1), true, false),
        0x65 => sse(c, SseOp::CmpGt(2), true, false),
        0x66 => sse(c, SseOp::CmpGt(4), true, false),
        0x74 => sse(c, SseOp::CmpEq(1), true, false),
        0x75 => sse(c, SseOp::CmpEq(2), true, false),
        0x76 => sse(c, SseOp::CmpEq(4), true, false),
        0x70 => sse(c, SseOp::Shufd, true, true),
        0x71..=0x73 => {
            let m = c.modrm()?;
            let imm = c.r.imm(1)? & 0xff;
            let elem = [2, 4, 8][(b - 0x71) as usize];
            let op = match (b, m.reg & 7) {
                (_, 2) => SseOp::Srl(elem),
                (0x71 | 0x72, 4) => SseOp::Sra(elem),
                (_, 6) => SseOp::Sll(elem),
                (0x73, 3) => SseOp::Srldq,
                (0x73, 7) => SseOp::Slldq,
                _ => return Err(DecodeError::Unsupported(b)),
            };
            c.done(Op::Sse(op), 16, xmm(m.rm), Operand::Imm(imm), imm)
        }
        0xd1 => sse(c, SseOp::Srl(2), true, false),
        0xd2 => sse(c, SseOp::Srl(4), true, false),
        0xd3 => sse(c, SseOp::Srl(8), true, false),
        0xe1 => sse(c, SseOp::Sra(2), true, false),
        0xe2 => sse(c, SseOp::Sra(4), true, false),
        0xf1 => sse(c, SseOp::Sll(2), true, false),
        0xf2 => sse(c, SseOp::Sll(4), true, false),
        0xf3 => sse(c, SseOp::Sll(8), true, false),
        0xd4 => sse(c, SseOp::Add(8), true, false),
        0xfc => sse(c, SseOp::Add(1), true, false),
        0xfd => sse(c, SseOp::Add(2), true, false),
        0xfe => sse(c, SseOp::Add(4), true, false),
        0xfb => sse(c, SseOp::Sub(8), true, false),
        0xf8 => sse(c, SseOp::Sub(1), true, false),
        0xf9 => sse(c, SseOp::Sub(2), true, false),
        0xfa => sse(c, SseOp::Sub(4), true, false),
        0xf4 => sse(c, SseOp::MulUdq, true, false),
        0xdb => sse(c, SseOp::And, true, false),
        0xdf => sse(c, SseOp::Andn, true, false),
        0xeb => sse(c, SseOp::Or, true, false),
        0xef => sse(c, SseOp::Xor, true, false),
        _ => Err(DecodeError::Unsupported(b)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mem(base: Option<u8>, index: Option<(u8, u8)>, disp: i64) -> Operand {
        Operand::Mem(MemRef {
            base,
            index,
            disp,
            rip: false,
            seg: Seg::None,
        })
    }

    #[test]
    fn decodes_basics() {
        let i = decode(&[0x48, 0x01, 0xd8]).unwrap();
        assert_eq!(
            (i.op, i.dst, i.src, i.size),
            (Op::Alu(AluOp::Add), Operand::Reg(0), Operand::Reg(3), 8)
        );
        let i = decode(&[0x41, 0x8b, 0x44, 0x24, 0x08]).unwrap(); // mov eax,[r12+8]
        assert_eq!(i.len, 5);
        assert_eq!(i.src, mem(Some(12), None, 8));
        let i = decode(&[0x48, 0x83, 0xec, 0x20]).unwrap(); // sub rsp,32
        assert_eq!((i.dst, i.src), (Operand::Reg(4), Operand::Imm(32)));
        let i = decode(&[0x48, 0x8d, 0x04, 0x8b]).unwrap(); // lea rax,[rbx+rcx*4]
        assert_eq!(i.src, mem(Some(3), Some((1, 4)), 0));
        assert_eq!(decode(&[0x7e, 0xfe]).unwrap().op, Op::Jcc(14));
    }

    #[test]
    fn byte_and_word_registers() {
        let i = decode(&[0x88, 0xe0]).unwrap(); // mov al, ah
        assert_eq!(
            (i.size, i.dst, i.src),
            (1, Operand::Reg(0), Operand::RegH(0))
        );
        let i = decode(&[0x40, 0x88, 0xe0]).unwrap(); // mov al, spl
        assert_eq!(i.src, Operand::Reg(4));
        let i = decode(&[0x66, 0x89, 0xc8]).unwrap(); // mov ax, cx
        assert_eq!(i.size, 2);
        let i = decode(&[0x66, 0xb8, 0x34, 0x12]).unwrap(); // mov ax, 0x1234
        assert_eq!((i.len, i.src), (4, Operand::Imm(0x1234)));
    }

    #[test]
    fn prefixes_and_maps() {
        let i = decode(&[0x64, 0x48, 0x8b, 0x04, 0x25, 0, 0, 0, 0]).unwrap(); // mov rax, fs:[0]
        let Operand::Mem(m) = i.src else { panic!() };
        assert_eq!((m.seg, m.base, i.len), (Seg::Fs, None, 9));
        let i = decode(&[0xf3, 0x48, 0x0f, 0xbc, 0xc1]).unwrap(); // tzcnt rax, rcx
        assert_eq!(i.op, Op::Bits(BitOp::Tzcnt));
        let i = decode(&[0x66, 0x0f, 0x70, 0xc1, 0x1b]).unwrap(); // pshufd xmm0, xmm1, 0x1b
        assert_eq!(
            (i.op, i.dst, i.src, i.imm),
            (
                Op::Sse(SseOp::Shufd),
                Operand::Xmm(0),
                Operand::Xmm(1),
                0x1b
            )
        );
        let i = decode(&[0x66, 0x2e, 0x0f, 0x1f, 0x84, 0, 0, 0, 0, 0]).unwrap(); // nop
        assert_eq!((i.op, i.len), (Op::Nop, 10));
        let i = decode(&[0xf3, 0x48, 0xab]).unwrap(); // rep stosq
        assert_eq!((i.op, i.size, i.rep), (Op::Stos, 8, Rep::Rep));
    }

    #[test]
    fn errors() {
        assert_eq!(decode(&[0x48]), Err(DecodeError::Truncated));
        assert_eq!(decode(&[0x01]), Err(DecodeError::Truncated));
        assert_eq!(decode(&[0x0f, 0x05]), Err(DecodeError::Unsupported(0x05)));
        assert_eq!(decode(&[0xd8, 0xc0]), Err(DecodeError::Unsupported(0xd8)));
    }
}
