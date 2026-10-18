//! The few Maxwell (SM 5.x) instructions the built-in kernels use,
//! encoded as Mesa's NAK compiler does (src/nouveau/compiler/nak/sm50.rs).
//! Every group of three instructions follows a 64-bit scheduling word:
//! stall cycles, scoreboards set and waited on. Checked against envyas
//! output in Mesa (gallium/drivers/nouveau/codegen/lib/gm107.asm.h):
//! `cd shader && cargo test`.

// Encoders the kernels do not use yet stay, checked against envyas.
#![cfg_attr(not(test), allow(dead_code))]

use alloc::vec::Vec;

/// Zero register and always-true predicate.
pub const RZ: u8 = 255;
pub const PT: u8 = 7;
/// Special registers (S2R).
pub const SR_TID_X: u8 = 0x21;
pub const SR_CTAID_X: u8 = 0x25;
/// SM cycle counter, low word (Mesa nak_private.h `NAK_SV_CLOCK_LO`).
pub const SR_CLOCKLO: u8 = 0x50;

/// Scheduling of one instruction: stall cycles before the next issue,
/// scoreboard released when its result is written (`wr`) or its sources
/// read (`rd`), scoreboards to wait for first (mask).
#[derive(Clone, Copy)]
pub struct Sched(u64);

impl Sched {
    pub const fn stall(n: u8) -> Self {
        Self(n as u64 & 0xF | 7 << 5 | 7 << 8)
    }

    pub const fn wr(self, sb: u8) -> Self {
        Self(self.0 & !(7 << 5) | (sb as u64) << 5)
    }

    pub const fn rd(self, sb: u8) -> Self {
        Self(self.0 & !(7 << 8) | (sb as u64) << 8)
    }

    pub const fn wait(self, mask: u8) -> Self {
        Self(self.0 | (mask as u64 & 0x3F) << 11)
    }

    pub const fn stalls(self) -> u8 {
        (self.0 & 0xF) as u8
    }

    pub const fn with_stall(self, n: u8) -> Self {
        Self(self.0 & !0xF | (n as u64 & 0xF))
    }

    /// Scoreboard this instruction sets when its result is written
    /// (`None`: none).
    pub const fn wr_bar(self) -> Option<u8> {
        let b = (self.0 >> 5 & 7) as u8;
        if b == 7 {
            None
        } else {
            Some(b)
        }
    }

    pub const fn rd_bar(self) -> Option<u8> {
        let b = (self.0 >> 8 & 7) as u8;
        if b == 7 {
            None
        } else {
            Some(b)
        }
    }

    pub const fn wait_mask(self) -> u8 {
        (self.0 >> 11 & 0x3F) as u8
    }
}

/// Integer comparisons (ISETP).
#[derive(Clone, Copy)]
pub enum Cmp {
    Lt = 1,
    Eq = 2,
    Le = 3,
    Gt = 4,
    Ne = 5,
    Ge = 6,
}

fn op(opcode: u64) -> u64 {
    opcode << 48 | (PT as u64) << 16
}

fn reg(r: u8, at: u32) -> u64 {
    (r as u64) << at
}

/// Constant buffer 0 operand at bits 20-38.
fn cb(off: u32) -> u64 {
    assert!(off.is_multiple_of(4) && off < 0x10000);
    ((off as u64) >> 2) << 20
}

/// Constant buffer `idx` operand: offset in words at bits 20-33, buffer
/// at 34-38 (NAK `set_src_cb`).
fn cbi(idx: u8, off: u32) -> u64 {
    assert!(idx < 32);
    cb(off) | (idx as u64) << 34
}

/// 20-bit signed immediate at bits 20-38, sign at 56.
fn imm20(v: i32) -> u64 {
    assert!((-(1 << 19)..(1 << 19)).contains(&v));
    ((v as u32 as u64) & 0x7FFFF) << 20 | ((v < 0) as u64) << 56
}

/// Run only if predicate `p` (inverted if `not`) is true.
pub fn when(insn: u64, p: u8, not: bool) -> u64 {
    insn & !(0xF << 16) | (p as u64) << 16 | (not as u64) << 19
}

pub fn nop() -> u64 {
    op(0x50B0) | 0xF << 8
}

pub fn exit() -> u64 {
    op(0xE300) | 0xF
}

pub fn s2r(d: u8, sr: u8) -> u64 {
    op(0xF0C8) | reg(d, 0) | reg(sr, 20)
}

pub fn mov(d: u8, a: u8) -> u64 {
    op(0x5C98) | reg(d, 0) | reg(a, 20) | 0xF << 39
}

pub fn mov_c(d: u8, off: u32) -> u64 {
    op(0x4C98) | reg(d, 0) | cb(off) | 0xF << 39
}

pub fn mov32i(d: u8, v: u32) -> u64 {
    op(0x0100) | reg(d, 0) | (v as u64) << 20 | 0xF << 12
}

pub fn shl(d: u8, a: u8, n: i32) -> u64 {
    op(0x3848) | reg(d, 0) | reg(a, 8) | imm20(n)
}

pub fn iadd(d: u8, a: u8, b: u8) -> u64 {
    op(0x5C10) | reg(d, 0) | reg(a, 8) | reg(b, 20)
}

pub fn iadd_i(d: u8, a: u8, v: i32) -> u64 {
    op(0x3810) | reg(d, 0) | reg(a, 8) | imm20(v)
}

pub fn iadd32i(d: u8, a: u8, v: i32) -> u64 {
    op(0x1C00) | reg(d, 0) | reg(a, 8) | (v as u32 as u64) << 20
}

/// IADD with a constant: `carry` sets CC (low half of a 64-bit add),
/// `x` adds CC in (high half).
pub fn iadd_c(d: u8, a: u8, off: u32, carry: bool, x: bool) -> u64 {
    op(0x4C10) | reg(d, 0) | reg(a, 8) | cb(off) | (x as u64) << 43 | (carry as u64) << 47
}

/// ISETP against `c[idx][off]`.
pub fn isetp_cb(p: u8, a: u8, idx: u8, off: u32, cmp: Cmp, signed: bool) -> u64 {
    isetp(0x4B60, p, a, cmp, signed) | cbi(idx, off)
}

/// ISETP.cmp.AND p, PT, a, b, PT (u32 unless `signed`).
fn isetp(opcode: u64, p: u8, a: u8, cmp: Cmp, signed: bool) -> u64 {
    op(opcode)
        | PT as u64
        | (p as u64) << 3
        | reg(a, 8)
        | (PT as u64) << 39
        | (signed as u64) << 48
        | (cmp as u64) << 49
}

pub fn isetp_r(p: u8, a: u8, b: u8, cmp: Cmp, signed: bool) -> u64 {
    isetp(0x5B60, p, a, cmp, signed) | reg(b, 20)
}

pub fn isetp_i(p: u8, a: u8, v: i32, cmp: Cmp, signed: bool) -> u64 {
    isetp(0x3660, p, a, cmp, signed) | imm20(v)
}

pub fn isetp_c(p: u8, a: u8, off: u32, cmp: Cmp, signed: bool) -> u64 {
    isetp(0x4B60, p, a, cmp, signed) | cb(off)
}

/// d = a * b + c, round to nearest.
pub fn ffma(d: u8, a: u8, b: u8, c: u8) -> u64 {
    op(0x5980) | reg(d, 0) | reg(a, 8) | reg(b, 20) | reg(c, 39)
}

/// d = a * c[0][off] + c.
pub fn ffma_c(d: u8, a: u8, off: u32, c: u8) -> u64 {
    op(0x4980) | reg(d, 0) | reg(a, 8) | cb(off) | reg(c, 39)
}

/// MUFU.RCP: d = 1 / a.
pub fn mufu_rcp(d: u8, a: u8) -> u64 {
    op(0x5080) | reg(d, 0) | reg(a, 8) | 4 << 20
}

/// d = a * c[idx][off].
pub fn fmul_c(d: u8, a: u8, idx: u8, off: u32) -> u64 {
    op(0x4C68) | reg(d, 0) | reg(a, 8) | cbi(idx, off)
}

/// d = a * b, saturated to 0..1 if `sat`.
pub fn fmul(d: u8, a: u8, b: u8, sat: bool) -> u64 {
    op(0x5C68) | reg(d, 0) | reg(a, 8) | reg(b, 20) | (sat as u64) << 50
}

/// d = a + c[idx][off].
pub fn fadd_c(d: u8, a: u8, idx: u8, off: u32) -> u64 {
    op(0x4C58) | reg(d, 0) | reg(a, 8) | cbi(idx, off)
}

/// d = a * c[idx][off] + c.
pub fn ffma_cb(d: u8, a: u8, idx: u8, off: u32, c: u8) -> u64 {
    op(0x4980) | reg(d, 0) | reg(a, 8) | cbi(idx, off) | reg(c, 39)
}

/// d = c[idx][off].
pub fn mov_cb(d: u8, idx: u8, off: u32) -> u64 {
    op(0x4C98) | reg(d, 0) | cbi(idx, off) | 0xF << 39
}

/// 32-bit global load / store, 64-bit address in a (even) register pair.
pub fn ldg(d: u8, a: u8) -> u64 {
    op(0xEED0) | reg(d, 0) | reg(a, 8) | 1 << 45 | 4 << 48
}

pub fn stg(a: u8, v: u8) -> u64 {
    op(0xEED8) | reg(v, 0) | reg(a, 8) | 1 << 45 | 4 << 48
}

/// ALD: `comps` (1-4) words of attribute `addr` into `d`, of this
/// vertex (a vertex shader's input).
pub fn ald(d: u8, addr: u32, comps: u8) -> u64 {
    assert!(addr < 0x400 && (1..=4).contains(&comps));
    op(0xEFD8)
        | reg(d, 0)
        | reg(RZ, 8)
        | (addr as u64) << 20
        | reg(RZ, 39)
        | ((comps - 1) as u64) << 47
}

/// AST: `comps` words from `d` to output attribute `addr`.
pub fn ast(d: u8, addr: u32, comps: u8) -> u64 {
    assert!(addr < 0x400 && (1..=4).contains(&comps));
    op(0xEFF0)
        | reg(d, 0)
        | reg(RZ, 8)
        | (addr as u64) << 20
        | 1 << 32
        | reg(RZ, 39)
        | ((comps - 1) as u64) << 47
}

/// Interpolation of a fragment shader's inputs (IPA).
#[derive(Clone, Copy)]
pub enum Interp {
    /// Screen-linear: the plane equation as it stands.
    Pass = 0,
    /// Perspective-correct: multiplied by the 1/w in `inv_w`.
    PassMulW = 1,
    /// The provoking vertex's value.
    Constant = 2,
}

/// IPA: input `addr` of the fragment (one word) into `d`; `inv_w` holds
/// the interpolated 1/w for `PassMulW`, RZ otherwise.
pub fn ipa(d: u8, addr: u32, how: Interp, inv_w: u8) -> u64 {
    assert!(addr.is_multiple_of(4) && addr < 0x400);
    op(0xE000)
        | reg(d, 0)
        | reg(RZ, 8)
        | reg(inv_w, 20)
        | (addr as u64) << 28
        | (PT as u64) << 47
        | reg(RZ, 39)
        | (how as u64) << 54
}

/// TEX.B (bindless, 2D, automatic LOD, all four channels): `d` = RGBA
/// texel (four registers) of the texture named by the handle in `handle`,
/// sampled at (`coords`, `coords` + 1), normalized. The handle is the
/// header index, plus the sampler index << 20; it sits in a vector of its
/// own (three registers, the others unused), as Gallium's gm107 emitter
/// and register allocator place it (NAK puts it ahead of the coordinates).
pub fn tex_b(d: u8, coords: u8, handle: u8) -> u64 {
    const DIM_2D: u64 = 2;
    op(0xDEB8) | reg(d, 0) | reg(coords, 8) | reg(handle, 20) | DIM_2D << 28 | 0xF << 31
}

// Encoders of the shader compiler (`lower`). Opcodes and fields as NAK's
// sm50.rs places them; the registers of vector operands are consecutive.

/// FADD d = a + b (`neg_b`: a - b), saturated to 0..1 if `sat`.
pub fn fadd(d: u8, a: u8, b: u8, neg_b: bool, sat: bool) -> u64 {
    op(0x5C58) | reg(d, 0) | reg(a, 8) | reg(b, 20) | (neg_b as u64) << 45 | (sat as u64) << 50
}

/// FMNMX: the smaller (`min`) or the larger of a and b; a NaN loses.
pub fn fmnmx(d: u8, a: u8, b: u8, min: bool) -> u64 {
    op(0x5C60) | reg(d, 0) | reg(a, 8) | reg(b, 20) | (PT as u64) << 39 | (!min as u64) << 42
}

#[derive(Clone, Copy)]
pub enum Logic {
    And = 0,
    Or = 1,
    Xor = 2,
    /// `b` alone (use RZ for `a`).
    PassB = 3,
}

/// LOP d = a op b, either operand complemented first on request.
pub fn lop(d: u8, a: u8, b: u8, how: Logic, not_a: bool, not_b: bool) -> u64 {
    op(0x5C40)
        | reg(d, 0)
        | reg(a, 8)
        | reg(b, 20)
        | (not_a as u64) << 39
        | (not_b as u64) << 40
        | (how as u64) << 41
        | (PT as u64) << 48
}

/// LOP32I d = a op imm (And, Or, Xor).
pub fn lop32i(d: u8, a: u8, imm: u32, how: Logic) -> u64 {
    assert!(!matches!(how, Logic::PassB));
    op(0x0400) | reg(d, 0) | reg(a, 8) | (imm as u64) << 20 | (how as u64) << 53
}

/// SHL d = a << (b mod 32).
pub fn shl_r(d: u8, a: u8, b: u8) -> u64 {
    op(0x5C48) | reg(d, 0) | reg(a, 8) | reg(b, 20) | 1 << 39
}

/// SHR d = a >> (b mod 32), arithmetic if `signed`.
pub fn shr_r(d: u8, a: u8, b: u8, signed: bool) -> u64 {
    op(0x5C28) | reg(d, 0) | reg(a, 8) | reg(b, 20) | 1 << 39 | (signed as u64) << 48
}

/// IMUL d = low 32 bits of a * b.
pub fn imul(d: u8, a: u8, b: u8) -> u64 {
    op(0x5C38) | reg(d, 0) | reg(a, 8) | reg(b, 20)
}

/// IMNMX: min or max, signed or unsigned.
pub fn imnmx(d: u8, a: u8, b: u8, min: bool, signed: bool) -> u64 {
    op(0x5C20)
        | reg(d, 0)
        | reg(a, 8)
        | reg(b, 20)
        | (PT as u64) << 39
        | (!min as u64) << 42
        | (signed as u64) << 48
}

/// IADD d = a - b.
pub fn isub(d: u8, a: u8, b: u8) -> u64 {
    op(0x5C10) | reg(d, 0) | reg(a, 8) | reg(b, 20) | 1 << 48
}

/// I2I.S32 d = |a|.
pub fn iabs(d: u8, a: u8) -> u64 {
    op(0x5CE0) | reg(d, 0) | reg(a, 20) | 2 << 8 | 2 << 10 | 1 << 12 | 1 << 13 | 1 << 49
}

/// F2F.F32.F32.FLOOR.
pub fn ffloor(d: u8, a: u8) -> u64 {
    op(0x5CA8) | reg(d, 0) | reg(a, 20) | 2 << 8 | 2 << 10 | 1 << 39 | 1 << 42
}

/// F2I.S32 / F2I.U32 of a float, truncating and saturating.
pub fn f2i(d: u8, a: u8, signed: bool) -> u64 {
    op(0x5CB0) | reg(d, 0) | reg(a, 20) | 2 << 8 | 2 << 10 | (signed as u64) << 12 | 3 << 39
}

/// I2F.F32.S32 / .U32.
pub fn i2f(d: u8, a: u8, signed: bool) -> u64 {
    op(0x5CB8) | reg(d, 0) | reg(a, 20) | 2 << 8 | 2 << 10 | (signed as u64) << 13
}

/// MUFU function codes.
pub const MUFU_COS: u8 = 0;
pub const MUFU_SIN: u8 = 1;
pub const MUFU_EX2: u8 = 2;
pub const MUFU_LG2: u8 = 3;
pub const MUFU_RCP: u8 = 4;
pub const MUFU_RSQ: u8 = 5;
pub const MUFU_SQRT: u8 = 8;

pub fn mufu(d: u8, a: u8, func: u8) -> u64 {
    op(0x5080) | reg(d, 0) | reg(a, 8) | (func as u64) << 20
}

/// RRO: range reduction before MUFU.SIN / COS (`exp2` false) or EX2.
pub fn rro(d: u8, a: u8, exp2: bool) -> u64 {
    op(0x5C90) | reg(d, 0) | reg(a, 20) | (exp2 as u64) << 39
}

/// Float comparisons (FSETP codes).
#[derive(Clone, Copy)]
pub enum FCmp {
    Lt = 0x01,
    Eq = 0x02,
    Le = 0x03,
    Gt = 0x04,
    Ge = 0x06,
    /// True with a NaN, as C's `!=`.
    NeU = 0x0D,
}

/// FSETP.cmp p = a cmp b.
pub fn fsetp(p: u8, a: u8, b: u8, cmp: FCmp) -> u64 {
    op(0x5BB0)
        | (PT as u64)
        | (p as u64) << 3
        | reg(a, 8)
        | reg(b, 20)
        | (PT as u64) << 39
        | (cmp as u64) << 48
}

/// ISETP.cmp.AND p = (a cmp b) && acc (a predicate, `not_acc` inverts it).
pub fn isetp_acc(p: u8, a: u8, b: u8, cmp: Cmp, signed: bool, acc: u8, not_acc: bool) -> u64 {
    isetp_r(p, a, b, cmp, signed) & !(0xF << 39) | (acc as u64) << 39 | (not_acc as u64) << 42
}

/// SEL d = pred ? a : b (`not` inverts the predicate).
pub fn sel(d: u8, a: u8, b: u8, pred: u8, not: bool) -> u64 {
    op(0x5CA0) | reg(d, 0) | reg(a, 8) | reg(b, 20) | (pred as u64) << 39 | (not as u64) << 42
}

#[derive(Clone, Copy)]
pub enum PLogic {
    And = 0,
    Or = 1,
    Xor = 2,
}

/// PSETP: d = ((s0 ^ n0) op0 (s1 ^ n1)) op1 (s2 ^ n2); `d2` takes the
/// same with the first term inverted (PT: dropped).
#[allow(clippy::too_many_arguments)]
pub fn psetp(
    d: u8,
    d2: u8,
    s0: (u8, bool),
    op0: PLogic,
    s1: (u8, bool),
    op1: PLogic,
    s2: (u8, bool),
) -> u64 {
    op(0x5090)
        | (d2 as u64)
        | (d as u64) << 3
        | (s0.0 as u64) << 12
        | (s0.1 as u64) << 15
        | (op0 as u64) << 24
        | (s1.0 as u64) << 29
        | (s1.1 as u64) << 32
        | (s2.0 as u64) << 39
        | (s2.1 as u64) << 42
        | (op1 as u64) << 45
}

/// KIL: the thread stops, its pixel is dropped.
pub fn kil() -> u64 {
    op(0xE330) | 0xF
}

pub fn sync() -> u64 {
    op(0xF0F8) | 0xF
}

pub fn brk() -> u64 {
    op(0xE340) | 0xF
}

pub fn cont() -> u64 {
    op(0xE350) | 0xF
}

/// A control op with its target still to patch (`Asm::mark_target`).
pub fn op_word(opcode: u64) -> u64 {
    op(opcode) | 0xF
}

/// The control ops with a target (patched by `Asm::ctl`).
pub const BRA: u64 = 0xE240;
pub const SSY: u64 = 0xE290;
pub const PBK: u64 = 0xE2A0;
pub const PCNT: u64 = 0xE2B0;

/// LDG / STG of 1, 2 or 4 words (consecutive registers, aligned to the
/// width), 64-bit address in the even pair `a`.
pub fn ldg_n(d: u8, a: u8, words: u8) -> u64 {
    op(0xEED0) | reg(d, 0) | reg(a, 8) | 1 << 45 | (mem_type(words) as u64) << 48
}

pub fn stg_n(a: u8, v: u8, words: u8) -> u64 {
    op(0xEED8) | reg(v, 0) | reg(a, 8) | 1 << 45 | (mem_type(words) as u64) << 48
}

fn mem_type(words: u8) -> u8 {
    match words {
        1 => 4,
        2 => 5,
        4 => 6,
        _ => panic!("access width"),
    }
}

/// ATOM.ADD.U32 d = [a]; [a] += v.
pub fn atom_add(d: u8, a: u8, v: u8) -> u64 {
    op(0xED00) | reg(d, 0) | reg(a, 8) | reg(v, 20) | 1 << 48
}

/// LDC d = c[idx][off + r]: a constant buffer word picked by a register
/// (a byte offset).
pub fn ldc(d: u8, r: u8, idx: u8, off: u32) -> u64 {
    assert!(off < 1 << 16 && idx < 32);
    op(0xEF90) | reg(d, 0) | reg(r, 8) | (off as u64) << 20 | (idx as u64) << 36 | 4 << 48
}

/// IADD with the 64-bit constant `c[idx][off]`: low half (`high` false,
/// sets the carry) or high half (adds it in).
pub fn iadd_cb(d: u8, a: u8, idx: u8, off: u32, high: bool) -> u64 {
    op(0x4C10)
        | reg(d, 0)
        | reg(a, 8)
        | cbi(idx, off)
        | (high as u64) << 43
        | (!high as u64) << 47
}

/// TEX.B with an explicit level: the coordinates and the level are in
/// consecutive registers from `coords`.
pub fn tex_b_lod(d: u8, coords: u8, handle: u8) -> u64 {
    tex_b(d, coords, handle) | 3 << 37
}

/// A position in the code a branch can target.
#[derive(Clone, Copy)]
pub struct Label(usize);

/// Instructions in groups of three, each group after its scheduling word.
pub struct Asm {
    insns: Vec<(u64, Sched)>,
    labels: Vec<usize>,
    /// Branches to patch: instruction index, label.
    fixups: Vec<(usize, Label)>,
}

impl Default for Asm {
    fn default() -> Self {
        Self::new()
    }
}

impl Asm {
    pub fn new() -> Self {
        Self {
            insns: Vec::new(),
            labels: Vec::new(),
            fixups: Vec::new(),
        }
    }

    pub fn emit(&mut self, insn: u64, s: Sched) {
        self.insns.push((insn, s));
    }

    /// Here, padded to the start of a group (branches never land on a
    /// scheduling word).
    pub fn label(&mut self) -> Label {
        let l = self.new_label();
        self.place(l);
        l
    }

    /// A label to place later (a forward target).
    pub fn new_label(&mut self) -> Label {
        self.labels.push(usize::MAX);
        Label(self.labels.len() - 1)
    }

    /// Bind `l` here, padded as `label` does.
    pub fn place(&mut self, l: Label) {
        while !self.insns.len().is_multiple_of(3) {
            self.emit(nop(), Sched::stall(0));
        }
        self.labels[l.0] = self.insns.len();
    }

    /// A control op with a relative target (`BRA`, `SSY`, `PBK`, `PCNT`).
    pub fn ctl(&mut self, opcode: u64, l: Label, p: u8, not: bool, s: Sched) {
        self.fixups.push((self.insns.len(), l));
        self.emit(when(op(opcode) | 0xF, p, not), s);
    }

    /// Patch the target of the control op at index `i` (emitted with
    /// `op_word`) with label `l`.
    pub fn mark_target(&mut self, i: usize, l: Label) {
        self.fixups.push((i, l));
    }

    pub fn len(&self) -> usize {
        self.insns.len()
    }

    pub fn is_empty(&self) -> bool {
        self.insns.is_empty()
    }

    /// The scheduling of the last instruction emitted.
    pub fn last_sched(&mut self) -> Option<&mut Sched> {
        self.insns.last_mut().map(|(_, s)| s)
    }

    /// Branch to `l` if predicate `p` (inverted if `not`) holds.
    pub fn bra(&mut self, l: Label, p: u8, not: bool, s: Sched) {
        self.fixups.push((self.insns.len(), l));
        self.emit(when(op(0xE240) | 0xF, p, not), s);
    }

    /// Byte address of instruction `i` in the output.
    fn addr(i: usize) -> i64 {
        (i / 3 * 32 + 8 + i % 3 * 8) as i64
    }

    pub fn finish(mut self) -> Vec<u64> {
        for &(i, Label(l)) in &self.fixups {
            assert!(self.labels[l] != usize::MAX, "label never placed");
            let rel = Self::addr(self.labels[l]) - Self::addr(i) - 8;
            self.insns[i].0 |= ((rel as u64) & 0xFF_FFFF) << 20;
        }
        while !self.insns.len().is_multiple_of(3) {
            self.emit(nop(), Sched::stall(0));
        }
        let mut out = Vec::with_capacity(self.insns.len() / 3 * 4);
        for g in self.insns.chunks(3) {
            out.push(
                g.iter()
                    .enumerate()
                    .fold(0, |w, (k, (_, s))| w | s.0 << (21 * k)),
            );
            out.extend(g.iter().map(|(i, _)| *i));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Words from Mesa's gm107.asm.h for the same instructions.
    #[test]
    fn envyas_words() {
        assert_eq!(mov32i(3, 0xFFFF_FFFF), 0x010F_FFFF_FFF7_F003);
        assert_eq!(mov32i(0, 0xBF80_0000), 0x010B_F800_0007_F000);
        assert_eq!(mov(3, 0), 0x5C98_0780_0007_0003);
        assert_eq!(mov(0, RZ), 0x5C98_0780_0FF7_0000);
        assert_eq!(shl(4, 4, 0x14), 0x3848_0000_0147_0404);
        assert_eq!(iadd(4, 2, 3), 0x5C10_0000_0037_0204);
        assert_eq!(when(iadd_i(0, 0, 1), 0, false), 0x3810_0000_0010_0000);
        assert_eq!(iadd32i(3, 2, -1), 0x1C0F_FFFF_FFF7_0203);
        assert_eq!(iadd32i(7, 7, 0x3FF0_0000), 0x1C03_FF00_0007_0707);
        assert_eq!(isetp_r(0, 1, 2, Cmp::Ge, false), 0x5B6C_0380_0027_0107);
        assert_eq!(isetp_r(2, 0, RZ, Cmp::Lt, true), 0x5B63_0380_0FF7_0017);
        assert_eq!(isetp_r(0, 4, RZ, Cmp::Eq, true), 0x5B65_0380_0FF7_0407);
        assert_eq!(isetp_i(0, 3, 0x7FD, Cmp::Gt, false), 0x3668_0380_7FD7_0307);
        assert_eq!(isetp_i(1, 3, 2, Cmp::Le, false), 0x3666_0380_0027_030F);
        assert_eq!(ffma(5, 4, 5, 0), 0x5980_0000_0057_0405);
        assert_eq!(nop(), 0x50B0_0000_0007_0F00);
    }

    #[test]
    fn sched_words() {
        // (st 0xd wr 0x0 wt 0x3f) (st 0x1 wt 0x1) (st 0x6)
        let mut a = Asm::new();
        a.emit(nop(), Sched::stall(0xD).wr(0).wait(0x3F));
        a.emit(nop(), Sched::stall(1).wait(1));
        a.emit(nop(), Sched::stall(6));
        // (st 0x0) (st 0x0) (st 0x0), as the padding.
        a.emit(nop(), Sched::stall(0));
        let w = a.finish();
        assert_eq!(w.len(), 8);
        assert_eq!(w[0], 0x001F_9801_FC21_FF0D);
        assert_eq!(w[4], 0x001F_8000_FC00_07E0);
        // (st 0x6 wr 0x0 wt 0x1) (st 0x6 wr 0x0 wt 0x1) (st 0x6 wr 0x0 rd 0x1 wt 0x1)
        let mut a = Asm::new();
        let s = Sched::stall(6).wr(0).wait(1);
        a.emit(nop(), s);
        a.emit(nop(), s);
        a.emit(nop(), s.rd(1));
        assert_eq!(a.finish()[0], 0x0024_1801_E0C0_0F06);
    }

    #[test]
    fn branches() {
        // A loop back to its own group: -32 from the next instruction.
        let mut a = Asm::new();
        let top = a.label();
        a.emit(nop(), Sched::stall(1));
        a.bra(top, 0, true, Sched::stall(5));
        let w = a.finish();
        // Instruction 1 at byte 16; target 8: 8 - 16 - 8 = -16.
        assert_eq!(w[2] >> 20 & 0xFF_FFFF, (-16i64 as u64) & 0xFF_FFFF);
        assert_eq!(w[2] & 0xF_000F, 0x8_000F, "@!P0, CC.T");
        // Forward over one group.
        let mut a = Asm::new();
        let skip_bra = a.insns.len();
        a.emit(0, Sched::stall(0));
        a.fixups.push((skip_bra, Label(0)));
        a.insns[0].0 = op(0xE240) | 0xF;
        a.emit(nop(), Sched::stall(0));
        a.emit(nop(), Sched::stall(0));
        let l = a.label();
        a.emit(exit(), Sched::stall(0));
        assert_eq!(l.0, 0);
        let w = a.finish();
        // From 8 to 40: 40 - 8 - 8 = 24.
        assert_eq!(w[1] >> 20 & 0xFF_FFFF, 24);
        assert_eq!(w[5], exit());
    }

    #[test]
    fn texture_fields() {
        // TEX.B R24..R27, R16, R20: opcode 0xdeb8, coordinates at 8, handle
        // at 20, dim 2D at 28, mask RGBA at 31, automatic LOD.
        assert_eq!(
            tex_b(24, 16, 20),
            0xDEB8_0000_0007_0018 | 16 << 8 | 20 << 20 | 2 << 28 | 0xF << 31
        );
    }

    /// NAK's encodings for the forms envyas' library does not use.
    #[test]
    fn nak_fields() {
        assert_eq!(s2r(0, SR_TID_X), 0xF0C8_0000_0217_0000);
        assert_eq!(exit(), 0xE300_0000_0007_000F);
        // LDG.E.32 R3, [R4]; STG.E.32 [R6], R8
        assert_eq!(ldg(3, 4), 0xEED4_2000_0007_0403);
        assert_eq!(stg(6, 8), 0xEEDC_2000_0007_0608);
        // c[0x0][0x14]: word index 5 at bit 20.
        assert_eq!(mov_c(3, 0x14), 0x4C98_0780_0057_0003);
        assert_eq!(iadd_c(4, 2, 0, true, false), 0x4C10_8000_0007_0204);
        assert_eq!(iadd_c(5, RZ, 4, false, true), 0x4C10_0800_0017_FF05);
        assert_eq!(ffma_c(8, 3, 0x14, 8), 0x4980_0400_0057_0308);
        assert_eq!(isetp_c(0, 0, 0x10, Cmp::Ge, false), 0x4B6C_0380_0047_0007);
    }

    #[test]
    fn constant_buffer_index() {
        // c[1][0x10]: word 4 at bit 20, buffer 1 at bit 34.
        assert_eq!(mov_cb(3, 1, 0x10), mov_c(3, 0x10) | 1 << 34);
        assert_eq!(ffma_cb(8, 3, 0, 0x14, 8), ffma_c(8, 3, 0x14, 8));
        // FMUL.SAT sets bit 50, FADD with a constant uses 0x4c58.
        assert_eq!(fmul(1, 2, 3, true) >> 48, 0x5C68 | 1 << 2);
        assert_eq!(fadd_c(1, 2, 3, 4) >> 48, 0x4C58);
        // MUFU.RCP R0, R0: opcode 0x5080, function 4 at bit 20.
        assert_eq!(mufu_rcp(0, 0), 0x5080_0000_0047_0000);
    }

    /// Fields as NAK's sm50.rs places them (opcode 0xefd8, 0xeff0, 0xe000).
    #[test]
    fn attribute_fields() {
        // ALD.4 R4, a[0x90]: dst 0-7, offset and vertex RZ, addr 20-29.
        assert_eq!(
            ald(4, 0x90, 4),
            0xEFD8_0000_0007_0004 | 0xFF << 8 | 0x90 << 20 | 0xFF << 39 | 3 << 47
        );
        // AST.4 a[0x70], R0.
        assert_eq!(
            ast(0, 0x70, 4),
            0xEFF0_0000_0007_0000 | 0xFF << 8 | 0x70 << 20 | 1 << 32 | 0xFF << 39 | 3 << 47
        );
        // IPA R1, a[0x84], pass, no 1/w.
        assert_eq!(
            ipa(1, 0x84, Interp::Pass, RZ),
            0xE000_0000_0007_0001 | 0xFF << 8 | 0xFF << 20 | 0x84 << 28 | 7 << 47 | 0xFF << 39
        );
    }
}
