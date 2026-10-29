//! x86-64 instruction decoder: the general-purpose integer set, string moves, BMI1, MOVBE,
//! and SSE2..SSE4.2 / AVX1 integer and data-move instructions (`simd`), as compilers
//! emit them for the PS4's Jaguar (8/16/32/64-bit operands, legacy and VEX encodings).

mod simd;

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

/// SIMD operations on xmm (and, for AVX1 moves and logic, ymm) registers. `u8` payloads
/// are element sizes in bytes; `bool` payloads say signed.
///
/// Two-source operations read `vvvv` under VEX and the destination otherwise; one-source
/// operations (moves, shuffles, immediate shifts, extensions) read `src` only.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SseOp {
    /// Aligned move (movdqa/movaps/movapd/movntdq): a misaligned address faults.
    Mov,
    /// Unaligned move (movdqu/movups/movupd/lddqu).
    MovU,
    /// movd/movq into an xmm register (zero-extended) or out of one; `size` is 4 or 8.
    MovGpr,
    /// movq xmm, xmm/m64 (zero-extended) and movq m64, xmm.
    MovQ,
    /// movddup: the low quadword in both halves.
    MovDup,
    Xor,
    And,
    Andn,
    Or,
    Add(u8),
    Sub(u8),
    /// Saturating signed / unsigned add and subtract.
    AddS(u8),
    AddUS(u8),
    SubS(u8),
    SubUS(u8),
    CmpEq(u8),
    CmpGt(u8),
    Min(u8, bool),
    Max(u8, bool),
    /// Low half of each product (pmullw, pmulld).
    MulLo(u8),
    /// High half of 16-bit products (pmulhw, pmulhuw).
    MulHi(bool),
    /// Even 32-bit lanes to 64-bit products: pmuludq, pmuldq.
    MulUdq,
    MulDq,
    Abs(u8),
    /// Shift each element by the low quadword of `src`.
    Sll(u8),
    Srl(u8),
    Sra(u8),
    /// Shift `src`'s elements by `imm`.
    SllI(u8),
    SrlI(u8),
    SraI(u8),
    /// Whole-register byte shifts of `src` by `imm` (pslldq / psrldq).
    Slldq,
    Srldq,
    /// pshufd / pshuflw / pshufhw with `imm`; pshufb by a control register.
    Shufd,
    Shuflw,
    Shufhw,
    Shufb,
    /// palignr by `imm` bytes.
    Alignr,
    UnpackLo(u8),
    UnpackHi(u8),
    /// Narrow `u8`-byte elements with signed / unsigned saturation.
    PackSS(u8),
    PackUS(u8),
    /// pmovsx / pmovzx: from, to element sizes, signed.
    Movx(u8, u8, bool),
    /// pmovmskb: byte sign bits to a GPR.
    Movmskb,
    /// ptest: ZF and CF from `dst & src` and `!dst & src`.
    Ptest,
    /// pextr / pinsr of a `u8`-byte element, lane `imm`.
    Extract(u8),
    Insert(u8),
    /// vinsertf128 / vextractf128, half `imm & 1`.
    Insert128,
    Extract128,
    /// vzeroupper / vzeroall.
    ZeroUpper,
    ZeroAll,
    /// Floating-point arithmetic, compares (`imm`: predicate) and roundings, lane-wise.
    Fp(FpOp, Ty),
    /// Conversions between float formats and integers.
    Cvt(CvtOp),
    /// comiss / ucomiss / comisd / ucomisd: EFLAGS from a compare (`true`: signaling).
    Comis(Ty, bool),
    /// movss / movsd: merge the low element (register) or zero-extend it (load).
    MovS(Ty),
    /// movlps / movhps / movhlps / movlhps / movsldup / movshdup.
    MovLH(LhOp),
    /// shufps / shufpd with `imm`, per 128-bit lane.
    Shufp(Ty),
    /// movmskps / movmskpd: sign bits to a GPR.
    Movmsk(Ty),
    /// blendps / blendpd / pblendw: `u8`-byte elements picked by `imm` bits.
    Blend(u8),
    /// blendvps / blendvpd / pblendvb: picked by the mask register's element sign bits
    /// (xmm0 for legacy encodings, `imm` (the register number) under VEX).
    Blendv(u8),
    /// insertps with `imm`.
    Insertps,
    /// dpps / dppd with `imm`.
    Dp(Ty),
    /// vbroadcastss / sd / f128 of a `u8`-byte memory operand.
    Broadcast(u8),
    /// vperm2f128 with `imm`.
    Perm2f128,
    /// vpermilps / vpermilpd by a control vector (`V`) or by `imm` (`I`).
    PermilV(Ty),
    PermilI(Ty),
    /// F16C: vcvtph2ps, vcvtps2ph (rounding from `imm`).
    Cvtph2ps,
    Cvtps2ph,
}

/// Element type of a float op: packed or scalar, single or double.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ty {
    Ps,
    Pd,
    Ss,
    Sd,
}

impl Ty {
    pub fn is_scalar(self) -> bool {
        matches!(self, Ty::Ss | Ty::Sd)
    }
    pub fn is_double(self) -> bool {
        matches!(self, Ty::Pd | Ty::Sd)
    }
    /// Element size in bytes.
    pub fn width(self) -> u8 {
        if self.is_double() {
            8
        } else {
            4
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FpOp {
    Add,
    Sub,
    Mul,
    Div,
    Min,
    Max,
    Sqrt,
    /// Approximate reciprocal and reciprocal square root (rcpps, rsqrtps).
    Rcp,
    Rsqrt,
    /// cmpps with predicate `imm` (0..8 legacy, 0..32 VEX).
    Cmp,
    /// roundps with `imm` (mode, MXCSR select, precision suppression).
    Round,
    /// haddps / hsubps / addsubps.
    HAdd,
    HSub,
    AddSub,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CvtOp {
    /// Between float formats: cvtps2pd, cvtpd2ps, cvtss2sd, cvtsd2ss.
    Ps2Pd,
    Pd2Ps,
    Ss2Sd,
    Sd2Ss,
    /// Packed int32 <-> float (`true`: truncating).
    Dq2Ps,
    Ps2Dq(bool),
    Dq2Pd,
    Pd2Dq(bool),
    /// Scalar from a GPR or memory integer of `size` bytes.
    Si2Ss,
    Si2Sd,
    /// Scalar to a GPR of `size` bytes (`true`: truncating).
    Ss2Si(bool),
    Sd2Si(bool),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LhOp {
    /// movlps / movlpd load: low quadword from memory, high kept.
    LoadLo,
    LoadHi,
    StoreLo,
    StoreHi,
    /// movhlps: low = source's high; movlhps: high = source's low.
    HiToLo,
    LoToHi,
    /// movsldup / movshdup: even / odd singles duplicated.
    DupLo,
    DupHi,
}

/// BMI1 operations (VEX-encoded, on GPRs).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BmiOp {
    /// `dst = !vvvv & src`.
    Andn,
    /// `dst = bits of src`, start and length from `vvvv`.
    Bextr,
    /// `vvvv = src & (src - 1)`, `src ^ (src - 1)`, `src & -src`.
    Blsr,
    Blsmsk,
    Blsi,
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
    Bmi(BmiOp),
    /// Byte-swapping load or store.
    Movbe,
    /// SSE4.2 crc32 (Castagnoli) of a `u8`-byte source into a 32-bit accumulator.
    Crc32(u8),
    /// ldmxcsr (`true`) / stmxcsr of a 32-bit memory operand.
    Mxcsr(bool),
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
    /// VEX.vvvv: the extra source (or BLS* destination) register.
    pub vvvv: u8,
    /// `VEX` and `VEX_L` bits.
    pub vex: u8,
}

/// `Inst::vex`: VEX-encoded.
pub const VEX: u8 = 1;
/// `Inst::vex`: VEX.L = 1 (256-bit).
pub const VEX_L: u8 = 2;

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
    /// 0x66 seen (or VEX.pp = 1).
    opsz: bool,
    rep: Rep,
    vvvv: u8,
    /// `VEX` / `VEX_L` bits.
    vex: u8,
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
            vvvv: self.vvvv,
            vex: self.vex,
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
        vvvv: 0,
        vex: 0,
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
    if b == 0xc4 || b == 0xc5 {
        return vex(&mut c, b);
    }
    if b & 0xf0 == 0x40 {
        c.rex = b;
        b = c.r.u8()?;
    }
    if b == 0x0f {
        return match c.r.u8()? {
            0x38 => {
                let b3 = c.r.u8()?;
                simd::decode_0f38(&mut c, b3)
            }
            0x3a => {
                let b3 = c.r.u8()?;
                simd::decode_0f3a(&mut c, b3)
            }
            b2 => decode_0f(&mut c, b2),
        };
    }
    decode_1byte(&mut c, b)
}

/// A VEX prefix (C4: three bytes, C5: two), then its opcode in the map it selects.
fn vex(c: &mut Ctx, b: u8) -> Result<Inst, DecodeError> {
    // 66 / F2 / F3 before VEX is #UD (a REX before C4/C5 makes them plain opcodes, refused).
    if c.opsz || c.rep != Rep::None {
        return Err(DecodeError::Unsupported(b));
    }
    let b1 = c.r.u8()?;
    let (map, last) = if b == 0xc5 {
        c.rex = (!b1 >> 5) & 4;
        (1, b1)
    } else {
        let b2 = c.r.u8()?;
        c.rex = ((!b1 >> 5) & 7) | ((b2 >> 4) & 8);
        (b1 & 0x1f, b2)
    };
    c.vvvv = (!last >> 3) & 15;
    c.vex = VEX | if last & 4 != 0 { VEX_L } else { 0 };
    match last & 3 {
        1 => c.opsz = true,
        2 => c.rep = Rep::Rep,
        3 => c.rep = Rep::Repne,
        _ => {}
    }
    let op = c.r.u8()?;
    match map {
        1 => simd::decode_0f(c, op),
        2 => simd::decode_0f38(c, op),
        3 => simd::decode_0f3a(c, op),
        _ => Err(DecodeError::Unsupported(b)),
    }
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
            match (m.rm, m.reg & 7) {
                (Operand::Mem(_), 2 | 3) if !c.opsz && c.rep == Rep::None => {
                    c.done(Op::Mxcsr(m.reg & 7 == 2), 4, m.rm, NONE, 0)
                }
                (Operand::Reg(_), 5..) => c.done(Op::Nop, size, NONE, NONE, 0),
                _ => Err(DecodeError::Unsupported(b)),
            }
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
        0x0d => {
            // prefetchw
            c.modrm()?;
            c.done(Op::Nop, size, NONE, NONE, 0)
        }
        0xc3 if !c.opsz && c.rep == Rep::None => {
            // movnti: a plain store here.
            let m = c.modrm()?;
            if !matches!(m.rm, Operand::Mem(_)) {
                return Err(DecodeError::Unsupported(b));
            }
            c.done(Op::Mov, size, m.rm, Operand::Reg(m.reg), 0)
        }
        _ => simd::decode_0f(c, b),
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
    fn vex() {
        let i = decode(&[0xc5, 0xf1, 0xfe, 0xc2]).unwrap(); // vpaddd xmm0, xmm1, xmm2
        assert_eq!(
            (i.op, i.vvvv, i.vex, i.len),
            (Op::Sse(SseOp::Add(4)), 1, VEX, 4)
        );
        let i = decode(&[0xc4, 0x41, 0x7e, 0x6f, 0x07]).unwrap(); // vmovdqu ymm8, [r15]
        assert_eq!(
            (i.op, i.dst, i.vex),
            (Op::Sse(SseOp::MovU), Operand::Xmm(8), VEX | VEX_L)
        );
        let i = decode(&[0xc4, 0xe2, 0xe0, 0xf2, 0xc1]).unwrap(); // andn rax, rbx, rcx
        assert_eq!((i.op, i.size, i.vvvv), (Op::Bmi(BmiOp::Andn), 8, 3));
        // AVX2 (256-bit integer) is not Jaguar's: refused, as vpaddd ymm would #UD there.
        assert!(decode(&[0xc5, 0xf5, 0xfe, 0xc2]).is_err());
        // A legacy prefix before VEX, and a one-source op naming vvvv, are #UD.
        assert!(decode(&[0x66, 0xc5, 0xf1, 0xfe, 0xc2]).is_err());
        assert!(decode(&[0xc4, 0xe2, 0x69, 0x1c, 0xc0]).is_err());
    }

    #[test]
    fn errors() {
        assert_eq!(decode(&[0x48]), Err(DecodeError::Truncated));
        assert_eq!(decode(&[0x01]), Err(DecodeError::Truncated));
        assert_eq!(decode(&[0x0f, 0x05]), Err(DecodeError::Unsupported(0x05)));
        assert_eq!(decode(&[0xd8, 0xc0]), Err(DecodeError::Unsupported(0xd8)));
    }
}
