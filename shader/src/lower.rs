//! IR to Maxwell (SM 5.3) SASS. One program becomes the code of a
//! vertex, fragment or compute shader, with the program header the
//! 3D engine wants for the first two.
//!
//! The program talks to the service through constant buffers:
//! `CB_UNIFORMS` (the launch's uniforms), `CB_BUFFERS` (one 16-byte
//! descriptor per bound buffer: address low, high, length in words) and
//! `CB_TEXTURES` (one texture handle word per bound texture). Control
//! flow follows NAK's Maxwell scheme: SSY / SYNC around an `if`, PBK /
//! PCNT with BRK / CONT for loops (bounded by a counter); short
//! arithmetic bodies are predicated instead.

use alloc::vec::Vec;

use crate::emit::{Eff, Emitter};
use crate::ir::*;
use crate::regalloc::{allocate, Alloc, Loc};
use crate::sass::*;
use crate::validate::{validate, Info};

pub const CB_UNIFORMS: u8 = 1;
pub const CB_BUFFERS: u8 = 2;
pub const CB_TEXTURES: u8 = 3;
pub const BUFFER_DESC_BYTES: u32 = 16;
pub const SPH_WORDS: usize = 20;

/// Attribute space addresses (bytes).
const A_POSITION: u32 = 0x70;
const A_POSITION_W: u32 = 0x7C;
const A_GENERIC: u32 = 0x80;
const A_INSTANCE_ID: u32 = 0x2F8;
const A_VERTEX_ID: u32 = 0x2FC;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    Invalid(crate::validate::Error),
    Unsupported(&'static str),
    /// More registers than the hardware has.
    Registers,
}

pub struct Compiled {
    pub stage: Stage,
    pub code: Vec<u64>,
    /// The 3D engine's program header (zeros for compute).
    pub header: [u32; SPH_WORDS],
    /// Registers per thread.
    pub gprs: u32,
    /// Bytes of local memory per thread (a multiple of 16).
    pub local_bytes: u32,
    pub info: Info,
}

pub fn compile(p: &Program) -> Result<Compiled, Error> {
    let info = validate(p).map_err(Error::Invalid)?;
    let mut frag_targets = 0u8;
    for &(k, _) in &info.outputs {
        if let OutKind::Color(t) = k {
            frag_targets = frag_targets.max(t + 1);
        }
    }
    if info.outputs.iter().any(|&(k, _)| k == OutKind::Depth) {
        return Err(Error::Unsupported("depth output"));
    }
    let out_regs = 4 * frag_targets;
    let s0 = out_regs.div_ceil(4) * 4;
    let wants_w = p.stage == Stage::Fragment
        && info
            .inputs
            .iter()
            .any(|&(_, _, i)| i == Interp::Perspective);
    let w_reg = s0 + 16;
    let base = if wants_w { w_reg + 1 } else { w_reg };
    let al = allocate(p, base).map_err(|_| Error::Registers)?;
    let mut g = Gen {
        p,
        al,
        e: Emitter::new(),
        s0,
        w_reg: wants_w.then_some(w_reg),
        loop_idx: 0,
        guard: None,
        tz: crate::align::trailing_zeros(p),
        tp: 0,
    };
    // Colors not written read as zero.
    for t in 0..frag_targets {
        for c in 0..4u8 {
            if !info.outputs.contains(&(OutKind::Color(t), c)) {
                let r = 4 * t + c;
                g.fx(mov32i(r, 0), &[r], &[]);
            }
        }
    }
    if let Some(w) = g.w_reg {
        // Attributes arrive divided by w: w is the reciprocal of the
        // interpolated 1/w.
        g.vr(ipa(w, A_POSITION_W, IpaMode::Pass, RZ), &[w], &[]);
        g.vr(mufu(w, w, MUFU_RCP), &[w], &[w]);
    }
    g.body(&p.body)?;
    g.e.drain();
    g.e.emit(exit(), &Eff::ctl(&[]));
    let gprs = (g.al.gprs.max(base as u16) as u32).max(s0 as u32 + 16);
    if gprs > 254 {
        return Err(Error::Registers);
    }
    let code = g.e.finish();
    let mut header = match p.stage {
        Stage::Vertex => vertex_header(&info),
        Stage::Fragment => fragment_header(&info, wants_w),
        Stage::Compute => [0; SPH_WORDS],
    };
    if p.stage != Stage::Compute && p.local_words != 0 {
        // SHADER_LOCAL_MEMORY_LOW_SIZE, bits 32-55.
        setf(
            &mut header,
            32,
            (p.local_words as u32 * 4).next_multiple_of(16),
        );
        setf(&mut header, DOES_LOAD_OR_STORE, 1);
    }
    Ok(Compiled {
        stage: p.stage,
        code,
        header,
        gprs,
        local_bytes: (p.local_words as u32 * 4).next_multiple_of(16),
        info,
    })
}

/// Bytes per warp of the reconvergence stack's spill area (SSY / PBK
/// entries past the on-chip ones), in the program header and the compute
/// launch; the `gpu` service reserves it per warp. Without it a deeply
/// nested branch reconverges wrong on the console (not in the simulator).
pub const CRS_BYTES_PER_WARP: u32 = 0x800;

// Program header (SPH version 3), fields as `gfx.rs` and NAK's `sph.rs`.

fn setf(h: &mut [u32; SPH_WORDS], lo: usize, v: u32) {
    h[lo / 32] |= v << (lo % 32);
    if !lo.is_multiple_of(32) && v >> (32 - lo % 32) != 0 {
        h[lo / 32 + 1] |= v >> (32 - lo % 32);
    }
}

const KILLS_PIXELS: usize = 15;
const DOES_GLOBAL_STORE: usize = 16;
const DOES_LOAD_OR_STORE: usize = 26;

fn common_flags(h: &mut [u32; SPH_WORDS], info: &Info) {
    setf(h, 5, 3); // version
    setf(h, 17, 1); // SASS version
    if info.buffers != 0 {
        setf(h, DOES_LOAD_OR_STORE, 1);
    }
    if info.stores {
        setf(h, DOES_GLOBAL_STORE, 1);
    }
    if info.discards {
        setf(h, KILLS_PIXELS, 1);
    }
    setf(h, 96, CRS_BYTES_PER_WARP); // SHADER_LOCAL_MEMORY_CRS_SIZE
}

fn vertex_header(info: &Info) -> [u32; SPH_WORDS] {
    let mut h = [0u32; SPH_WORDS];
    setf(&mut h, 0, 1); // SPH type: VTG
    setf(&mut h, 10, 1); // vertex shader
    common_flags(&mut h, info);
    setf(&mut h, 140, 0xFF); // no store-request range
                             // System values read: instance and vertex number (IMAP system values C).
    if info.builtins >> Builtin::InstanceId as u32 & 1 != 0 {
        setf(&mut h, 336 + 14, 1);
    }
    if info.builtins >> Builtin::VertexId as u32 & 1 != 0 {
        setf(&mut h, 336 + 15, 1);
    }
    for &(attr, comp, _) in &info.inputs {
        let bit = 192 + 4 * attr as usize + comp as usize;
        h[bit / 32] |= 1 << (bit % 32);
    }
    for &(k, comp) in &info.outputs {
        let bit = match k {
            OutKind::Position => 428 + comp as usize,
            OutKind::Varying(n) => 432 + 4 * n as usize + comp as usize,
            _ => continue,
        };
        h[bit / 32] |= 1 << (bit % 32);
    }
    h
}

fn fragment_header(info: &Info, reads_w: bool) -> [u32; SPH_WORDS] {
    let mut h = [0u32; SPH_WORDS];
    setf(&mut h, 0, 2); // SPH type: PS
    setf(&mut h, 10, 5); // pixel shader
    setf(&mut h, 14, 1); // MRT enable
    common_flags(&mut h, info);
    // Position.w is always declared, as the built-in shaders do: a pixel
    // shader with no input at all hung an Erista's shader unit.
    let _ = reads_w;
    setf(&mut h, 191, 1);
    for &(attr, comp, i) in &info.inputs {
        let word = 4 * attr as usize + comp as usize;
        let code = match i {
            Interp::Flat => 1,
            Interp::Perspective => 2,
            Interp::Linear => 3,
        };
        let bit = 192 + 2 * word;
        h[bit / 32] |= code << (bit % 32);
    }
    for &(k, comp) in &info.outputs {
        if let OutKind::Color(t) = k {
            let bit = 576 + 4 * t as usize + comp as usize;
            h[bit / 32] |= 1 << (bit % 32);
        }
    }
    h
}

struct Gen<'a> {
    p: &'a Program,
    al: Alloc,
    e: Emitter,
    /// First scratch register: A (4), B (4), address pair, T0, T1,
    /// texture handle (3), T2.
    s0: u8,
    w_reg: Option<u8>,
    loop_idx: usize,
    guard: Option<(u8, bool)>,
    /// Known trailing zero bits of each register (`align`).
    tz: Vec<u8>,
    /// Which temporary predicate (P5 / P6) comes next.
    tp: u8,
}

/// Vector loads write their results where the allocator put them.
const IN_PLACE_LOADS: bool = true;
const P_TEMP_A: u8 = 5;
const P_TEMP_B: u8 = 6;

impl Gen<'_> {
    fn sa(&self) -> u8 {
        self.s0
    }

    fn sb(&self) -> u8 {
        self.s0 + 4
    }

    fn addr(&self) -> u8 {
        self.s0 + 8
    }

    fn t0(&self) -> u8 {
        self.s0 + 10
    }

    fn t1(&self) -> u8 {
        self.s0 + 11
    }

    fn handle(&self) -> u8 {
        self.s0 + 12
    }

    fn t2(&self) -> u8 {
        self.s0 + 15
    }

    fn emit(&mut self, insn: u64, e: Eff) {
        let insn = match self.guard {
            Some((p, not)) => when(insn, p, not),
            None => insn,
        };
        self.e.emit(insn, &e);
    }

    fn fx(&mut self, insn: u64, w: &[u8], r: &[u8]) {
        self.emit(insn, Eff::fixed(w, r));
    }

    fn vr(&mut self, insn: u64, w: &[u8], r: &[u8]) {
        self.emit(insn, Eff::var(w, r));
    }

    fn st(&mut self, insn: u64, r: &[u8]) {
        self.emit(insn, Eff::store(r));
    }

    fn loc(&self, r: Reg) -> Loc {
        self.al.locs[r.0 as usize]
    }

    fn ty(&self, r: Reg) -> Ty {
        self.p.reg_types[r.0 as usize]
    }

    /// The GPR of a numeric register.
    fn g(&self, r: Reg) -> u8 {
        match self.loc(r) {
            Loc::Gpr(x) => x,
            Loc::Pred(_) => unreachable!("predicate where a GPR is needed"),
        }
    }

    /// A predicate holding Bool register `r`: its own, or a temporary
    /// loaded from the GPR that holds it.
    fn pred_of(&mut self, r: Reg) -> u8 {
        match self.loc(r) {
            Loc::Pred(q) => q,
            Loc::Gpr(x) => {
                let t = P_TEMP_A + (self.tp & 1);
                self.tp ^= 1;
                self.emit(
                    isetp_r(t, x, RZ, Cmp::Ne, false),
                    Eff::fixed(&[], &[x]).preds(&[], &[t]),
                );
                t
            }
        }
    }

    /// A predicate to compute Bool register `r` into: its own or a
    /// temporary, to `commit` afterwards.
    fn pred_dst(&self, r: Reg) -> u8 {
        match self.loc(r) {
            Loc::Pred(q) => q,
            Loc::Gpr(_) => P_TEMP_B,
        }
    }

    /// If `r` lives in a GPR, set it from the temporary predicate.
    fn commit(&mut self, r: Reg) {
        if let Loc::Gpr(x) = self.loc(r) {
            self.fx(mov32i(x, 1), &[x], &[]);
            self.emit(
                when(mov(x, RZ), P_TEMP_B, true),
                Eff::fixed(&[x], &[]).preds(&[P_TEMP_B], &[]),
            );
        }
    }

    fn psetp(&mut self, d: u8, s0: (u8, bool), o0: PLogic, s1: (u8, bool), s2: (u8, bool)) {
        let srcs = [s0.0, s1.0, s2.0];
        let w = [d];
        self.emit(
            psetp(d, PT, s0, o0, s1, PLogic::And, s2),
            Eff::fixed(&[], &[]).preds(&srcs, &w),
        );
    }

    // Instruction lowering.

    fn body(&mut self, b: &[Inst]) -> Result<(), Error> {
        for i in b {
            self.inst(i)?;
        }
        Ok(())
    }

    fn inst(&mut self, i: &Inst) -> Result<(), Error> {
        match i {
            Inst::Const { dst, bits } => match self.loc(*dst) {
                Loc::Gpr(d) => self.fx(mov32i(d, *bits), &[d], &[]),
                Loc::Pred(q) => {
                    self.psetp(q, (PT, *bits == 0), PLogic::And, (PT, false), (PT, false))
                }
            },
            Inst::Mov { dst, src } => {
                if self.ty(*dst) == Ty::Bool {
                    let s = self.pred_of(*src);
                    let d = self.pred_dst(*dst);
                    self.psetp(d, (s, false), PLogic::And, (PT, false), (PT, false));
                    self.commit(*dst);
                } else {
                    let (d, a) = (self.g(*dst), self.g(*src));
                    self.fx(mov(d, a), &[d], &[a]);
                }
            }
            Inst::Un { op, dst, a } => self.un(*op, *dst, *a)?,
            Inst::Bin { op, dst, a, b } => self.bin(*op, *dst, *a, *b),
            Inst::Fma { dst, a, b, c } => {
                let (d, a, b, c) = (self.g(*dst), self.g(*a), self.g(*b), self.g(*c));
                self.fx(ffma(d, a, b, c), &[d], &[a, b, c]);
            }
            Inst::Sel { dst, p, a, b } => self.sel(*dst, *p, *a, *b),
            Inst::Cmp { op, dst, a, b } => {
                let (x, y) = (self.g(*a), self.g(*b));
                let d = self.pred_dst(*dst);
                let insn = match self.ty(*a) {
                    Ty::F32 => fsetp(
                        d,
                        x,
                        y,
                        match op {
                            CmpOp::Eq => FCmp::Eq,
                            CmpOp::Ne => FCmp::NeU,
                            CmpOp::Lt => FCmp::Lt,
                            CmpOp::Le => FCmp::Le,
                            CmpOp::Gt => FCmp::Gt,
                            CmpOp::Ge => FCmp::Ge,
                        },
                    ),
                    t => isetp_r(d, x, y, icmp(*op), t == Ty::I32),
                };
                self.emit(insn, Eff::fixed(&[], &[x, y]).preds(&[], &[d]));
                self.commit(*dst);
            }
            Inst::Input {
                dst,
                attr,
                comp,
                interp,
            } => {
                let d = self.g(*dst);
                let at = A_GENERIC + 16 * *attr as u32 + 4 * *comp as u32;
                if self.p.stage == Stage::Vertex {
                    self.vr(ald(d, at, 1), &[d], &[]);
                } else {
                    let (how, w) = match interp {
                        Interp::Perspective => (IpaMode::PassMulW, self.w_reg.unwrap()),
                        Interp::Linear => (IpaMode::Pass, RZ),
                        Interp::Flat => (IpaMode::Constant, RZ),
                    };
                    self.vr(ipa(d, at, how, w), &[d], &[w]);
                }
            }
            Inst::Output { kind, comp, src } => {
                let s = self.g(*src);
                match (self.p.stage, kind) {
                    (Stage::Vertex, OutKind::Position) => {
                        self.st(ast(s, A_POSITION + 4 * *comp as u32, 1), &[s])
                    }
                    (Stage::Vertex, OutKind::Varying(n)) => {
                        let at = A_GENERIC + 16 * *n as u32 + 4 * *comp as u32;
                        self.st(ast(s, at, 1), &[s]);
                    }
                    (Stage::Fragment, OutKind::Color(t)) => {
                        let d = 4 * *t + *comp;
                        self.fx(mov(d, s), &[d], &[s]);
                    }
                    _ => return Err(Error::Unsupported("output")),
                }
            }
            Inst::Builtin { dst, which } => {
                let d = self.g(*dst);
                match which {
                    Builtin::LocalIdX => self.vr(s2r(d, SR_TID_X), &[d], &[]),
                    Builtin::LocalIdY => self.vr(s2r(d, SR_TID_X + 1), &[d], &[]),
                    Builtin::LocalIdZ => self.vr(s2r(d, SR_TID_X + 2), &[d], &[]),
                    Builtin::GroupIdX => self.vr(s2r(d, SR_CTAID_X), &[d], &[]),
                    Builtin::GroupIdY => self.vr(s2r(d, SR_CTAID_X + 1), &[d], &[]),
                    Builtin::GroupIdZ => self.vr(s2r(d, SR_CTAID_X + 2), &[d], &[]),
                    Builtin::VertexId => self.vr(ald(d, A_VERTEX_ID, 1), &[d], &[]),
                    Builtin::InstanceId => self.vr(ald(d, A_INSTANCE_ID, 1), &[d], &[]),
                    Builtin::FragX | Builtin::FragY | Builtin::FragZ => {
                        return Err(Error::Unsupported("fragment position"))
                    }
                }
            }
            Inst::Uniform { dst, word } => {
                let d = self.g(*dst);
                self.fx(mov_cb(d, CB_UNIFORMS, 4 * *word as u32), &[d], &[]);
            }
            Inst::UniformIdx { dst, base, index } => {
                let (d, x, t) = (self.g(*dst), self.g(*index), self.t0());
                self.fx(shl(t, x, 2), &[t], &[x]);
                self.emit(
                    ldc(d, t, CB_UNIFORMS, 4 * *base as u32),
                    Eff::var(&[d], &[t]).read_barrier(),
                );
            }
            Inst::BufLoad {
                buf,
                index,
                offset,
                dsts,
            } => self.buf_load(*buf, *index, *offset, dsts),
            Inst::BufStore {
                buf,
                index,
                offset,
                srcs,
            } => self.buf_store(*buf, *index, *offset, srcs),
            Inst::BufAtomicAdd {
                dst,
                buf,
                index,
                src,
            } => {
                let d = self.g(*dst);
                let s = self.g(*src);
                self.buf_addr(*buf, *index, 0, 1);
                self.fx(mov(d, RZ), &[d], &[]);
                let a = self.addr();
                self.emit(
                    when(atom_add(d, a, s), P_TEMP_A, false),
                    Eff::var(&[d], &[a, a + 1, s])
                        .preds(&[P_TEMP_A], &[])
                        .read_barrier(),
                );
            }
            Inst::LocalLoad { dst, index } => {
                let (d, x, t) = (self.g(*dst), self.g(*index), self.t0());
                self.local_check(x, t);
                self.fx(mov(d, RZ), &[d], &[]);
                self.emit(
                    when(ldl(d, t, 0), P_TEMP_A, false),
                    Eff::var(&[d], &[t]).preds(&[P_TEMP_A], &[]).read_barrier(),
                );
            }
            Inst::LocalStore { index, src } => {
                let (x, s, t) = (self.g(*index), self.g(*src), self.t0());
                self.local_check(x, t);
                self.emit(
                    when(stl(t, s, 0), P_TEMP_A, false),
                    Eff::store(&[t, s]).preds(&[P_TEMP_A], &[]),
                );
            }
            Inst::Tex {
                tex,
                coords,
                lod,
                dsts,
            } => {
                let (sa, sb, h) = (self.sa(), self.sb(), self.handle());
                let u = self.g(coords[0]);
                let v = self.g(coords[1]);
                self.fx(mov(sa, u), &[sa], &[u]);
                self.fx(mov(sa + 1, v), &[sa + 1], &[v]);
                if let Some(l) = lod {
                    let l = self.g(*l);
                    self.fx(mov(sa + 2, l), &[sa + 2], &[l]);
                }
                self.fx(mov_cb(h, CB_TEXTURES, 4 * *tex as u32), &[h], &[]);
                let insn = if lod.is_some() {
                    tex_b_lod(sb, sa, h)
                } else {
                    tex_b(sb, sa, h)
                };
                let r: Vec<u8> = (0..3).map(|k| sa + k).chain([h]).collect();
                let w: Vec<u8> = (0..4).map(|k| sb + k).collect();
                self.vr(insn, &w, &r);
                for (k, d) in dsts.iter().enumerate() {
                    let d = self.g(*d);
                    self.fx(mov(d, sb + k as u8), &[d], &[sb + k as u8]);
                }
            }
            Inst::If { cond, then_, else_ } => self.if_(*cond, then_, else_)?,
            Inst::Loop { max_iter, body } => self.loop_(*max_iter, body)?,
            Inst::Break { cond } => self.jump(brk(), *cond),
            Inst::Continue { cond } => self.jump(cont(), *cond),
            Inst::Discard { cond } => self.jump(kil(), *cond),
        }
        Ok(())
    }

    /// BRK / CONT / KIL, when `cond` holds.
    fn jump(&mut self, insn: u64, cond: Option<Reg>) {
        match cond {
            Some(c) => {
                let p = self.pred_of(c);
                self.e.emit(when(insn, p, false), &Eff::ctl(&[p]));
            }
            None => self.e.emit(insn, &Eff::ctl(&[])),
        }
    }

    fn un(&mut self, op: UnOp, dst: Reg, a: Reg) -> Result<(), Error> {
        if op == UnOp::BNot {
            let s = self.pred_of(a);
            let d = self.pred_dst(dst);
            self.psetp(d, (s, true), PLogic::And, (PT, false), (PT, false));
            self.commit(dst);
            return Ok(());
        }
        let (d, x) = (self.g(dst), self.g(a));
        match op {
            UnOp::FNeg => self.fx(lop32i(d, x, 0x8000_0000, Logic::Xor), &[d], &[x]),
            UnOp::FAbs => self.fx(lop32i(d, x, 0x7FFF_FFFF, Logic::And), &[d], &[x]),
            UnOp::FRcp => self.vr(mufu(d, x, MUFU_RCP), &[d], &[x]),
            UnOp::FRsq => self.vr(mufu(d, x, MUFU_RSQ), &[d], &[x]),
            UnOp::FSqrt => self.vr(mufu(d, x, MUFU_SQRT), &[d], &[x]),
            UnOp::FLg2 => self.vr(mufu(d, x, MUFU_LG2), &[d], &[x]),
            UnOp::FSin | UnOp::FCos | UnOp::FEx2 => {
                let t = self.t2();
                let (exp2, f) = match op {
                    UnOp::FSin => (false, MUFU_SIN),
                    UnOp::FCos => (false, MUFU_COS),
                    _ => (true, MUFU_EX2),
                };
                self.vr(rro(t, x, exp2), &[t], &[x]);
                self.vr(mufu(d, t, f), &[d], &[t]);
            }
            UnOp::FFloor => self.vr(ffloor(d, x), &[d], &[x]),
            UnOp::FSat => self.fx(fadd(d, x, RZ, false, true), &[d], &[x]),
            UnOp::F2I => self.vr(f2i(d, x, true), &[d], &[x]),
            UnOp::F2U => self.vr(f2i(d, x, false), &[d], &[x]),
            UnOp::I2F => self.vr(i2f(d, x, true), &[d], &[x]),
            UnOp::U2F => self.vr(i2f(d, x, false), &[d], &[x]),
            UnOp::INeg => self.fx(isub(d, RZ, x), &[d], &[x]),
            UnOp::IAbs => self.vr(iabs(d, x), &[d], &[x]),
            UnOp::Not => self.fx(lop(d, RZ, x, Logic::PassB, false, true), &[d], &[x]),
            UnOp::FBits | UnOp::BitsF | UnOp::IBits | UnOp::UBits => self.fx(mov(d, x), &[d], &[x]),
            UnOp::BNot => unreachable!(),
        }
        Ok(())
    }

    fn bin(&mut self, op: BinOp, dst: Reg, a: Reg, b: Reg) {
        if matches!(op, BinOp::BAnd | BinOp::BOr | BinOp::BXor) {
            let (x, y) = (self.pred_of(a), self.pred_of(b));
            let d = self.pred_dst(dst);
            let o = match op {
                BinOp::BAnd => PLogic::And,
                BinOp::BOr => PLogic::Or,
                _ => PLogic::Xor,
            };
            self.psetp(d, (x, false), o, (y, false), (PT, false));
            self.commit(dst);
            return;
        }
        let (d, x, y) = (self.g(dst), self.g(a), self.g(b));
        let (w, r) = ([d], [x, y]);
        let signed = self.ty(a) == Ty::I32;
        match op {
            BinOp::FAdd => self.fx(fadd(d, x, y, false, false), &w, &r),
            BinOp::FSub => self.fx(fadd(d, x, y, true, false), &w, &r),
            BinOp::FMul => self.fx(fmul(d, x, y, false), &w, &r),
            BinOp::FMin => self.fx(fmnmx(d, x, y, true), &w, &r),
            BinOp::FMax => self.fx(fmnmx(d, x, y, false), &w, &r),
            BinOp::IAdd => self.fx(iadd(d, x, y), &w, &r),
            BinOp::ISub => self.fx(isub(d, x, y), &w, &r),
            BinOp::IMul => self.vr(imul(d, x, y), &w, &r),
            BinOp::IMin => self.fx(imnmx(d, x, y, true, signed), &w, &r),
            BinOp::IMax => self.fx(imnmx(d, x, y, false, signed), &w, &r),
            BinOp::And => self.fx(lop(d, x, y, Logic::And, false, false), &w, &r),
            BinOp::Or => self.fx(lop(d, x, y, Logic::Or, false, false), &w, &r),
            BinOp::Xor => self.fx(lop(d, x, y, Logic::Xor, false, false), &w, &r),
            BinOp::Shl => self.fx(shl_r(d, x, y), &w, &r),
            BinOp::Shr => self.fx(shr_r(d, x, y, false), &w, &r),
            BinOp::Sar => self.fx(shr_r(d, x, y, true), &w, &r),
            BinOp::BAnd | BinOp::BOr | BinOp::BXor => unreachable!(),
        }
    }

    fn sel(&mut self, dst: Reg, p: Reg, a: Reg, b: Reg) {
        if self.ty(dst) != Ty::Bool {
            let (d, x, y) = (self.g(dst), self.g(a), self.g(b));
            let q = self.pred_of(p);
            self.emit(
                sel(d, x, y, q, false),
                Eff::fixed(&[d], &[x, y]).preds(&[q], &[]),
            );
            return;
        }
        // Predicates: work on 0 / 1 words, then back.
        let (ta, tb, tr) = (self.t0(), self.t1(), self.t2());
        self.bool_word(a, ta);
        self.bool_word(b, tb);
        let q = self.pred_of(p);
        self.emit(
            sel(tr, ta, tb, q, false),
            Eff::fixed(&[tr], &[ta, tb]).preds(&[q], &[]),
        );
        match self.loc(dst) {
            Loc::Gpr(x) => self.fx(mov(x, tr), &[x], &[tr]),
            Loc::Pred(q) => self.emit(
                isetp_r(q, tr, RZ, Cmp::Ne, false),
                Eff::fixed(&[], &[tr]).preds(&[], &[q]),
            ),
        }
    }

    /// Bool register `r` as a 0 / 1 word in `t`.
    fn bool_word(&mut self, r: Reg, t: u8) {
        match self.loc(r) {
            Loc::Gpr(x) => self.fx(mov(t, x), &[t], &[x]),
            Loc::Pred(q) => {
                self.fx(mov32i(t, 1), &[t], &[]);
                self.emit(
                    when(mov(t, RZ), q, true),
                    Eff::fixed(&[t], &[]).preds(&[q], &[]),
                );
            }
        }
    }

    // Memory.

    /// `t` = the byte offset of local word `x`, and P5 whether it is one of
    /// the program's words.
    fn local_check(&mut self, x: u8, t: u8) {
        let words = self.p.local_words as i32;
        self.emit(
            isetp_i(P_TEMP_A, x, words, Cmp::Lt, false),
            Eff::fixed(&[], &[x]).preds(&[], &[P_TEMP_A]),
        );
        self.fx(shl(t, x, 2), &[t], &[x]);
    }

    /// Address of `buf[index + offset]` in the pair `addr()`, and in
    /// P5 whether the `n`-word access is aligned and inside.
    fn buf_addr(&mut self, buf: u8, index: Reg, offset: u16, n: u8) {
        let (t0, t1, ad) = (self.t0(), self.t1(), self.addr());
        let desc = BUFFER_DESC_BYTES * buf as u32;
        let mut i = self.g(index);
        if offset != 0 {
            self.fx(iadd32i(t0, i, offset as i32), &[t0], &[i]);
            i = t0;
        }
        self.emit(
            isetp_cb(P_TEMP_A, i, CB_BUFFERS, desc + 8, Cmp::Lt, false),
            Eff::fixed(&[], &[i]).preds(&[], &[P_TEMP_A]),
        );
        let aligned =
            self.tz[index.0 as usize] >= n.trailing_zeros() as u8 && offset as u32 % n as u32 == 0;
        if n > 1 && !aligned {
            self.fx(lop32i(t1, i, n as u32 - 1, Logic::And), &[t1], &[i]);
            self.emit(
                isetp_acc(P_TEMP_A, t1, RZ, Cmp::Eq, false, P_TEMP_A, false),
                Eff::fixed(&[], &[t1]).preds(&[P_TEMP_A], &[P_TEMP_A]),
            );
        }
        self.fx(shl(t1, i, 2), &[t1], &[i]);
        self.fx(iadd_cb(ad, t1, CB_BUFFERS, desc, false), &[ad], &[t1]);
        self.fx(
            iadd_cb(ad + 1, RZ, CB_BUFFERS, desc + 4, true),
            &[ad + 1],
            &[ad],
        );
    }

    fn buf_load(&mut self, buf: u8, index: Reg, offset: u16, dsts: &[Reg]) {
        let n = dsts.len() as u8;
        self.buf_addr(buf, index, offset, n);
        let ad = self.addr();
        // The allocator puts the results of a vector load in consecutive
        // registers, aligned to the width: written in place.
        let own: Vec<u8> = dsts.iter().map(|&d| self.g(d)).collect();
        let in_place = n == 1
            || IN_PLACE_LOADS
                && own[0] % n == 0
                && own.iter().enumerate().all(|(k, &r)| r == own[0] + k as u8);
        let blk = self.sb();
        let regs: Vec<u8> = if in_place {
            own.clone()
        } else {
            (0..n).map(|k| blk + k).collect()
        };
        for &r in &regs {
            self.fx(mov(r, RZ), &[r], &[]);
        }
        self.emit(
            when(ldg_n(regs[0], ad, n), P_TEMP_A, false),
            Eff::var(&regs, &[ad, ad + 1])
                .preds(&[P_TEMP_A], &[])
                .read_barrier(),
        );
        if !in_place {
            for (k, &d) in own.iter().enumerate() {
                self.fx(mov(d, blk + k as u8), &[d], &[blk + k as u8]);
            }
        }
    }

    fn buf_store(&mut self, buf: u8, index: Reg, offset: u16, srcs: &[Reg]) {
        let n = srcs.len() as u8;
        self.buf_addr(buf, index, offset, n);
        let ad = self.addr();
        let blk = self.sb();
        let regs: Vec<u8> = if n == 1 {
            alloc::vec![self.g(srcs[0])]
        } else {
            for (k, s) in srcs.iter().enumerate() {
                let s = self.g(*s);
                self.fx(mov(blk + k as u8, s), &[blk + k as u8], &[s]);
            }
            (0..n).map(|k| blk + k).collect()
        };
        let mut r = regs.clone();
        r.extend([ad, ad + 1]);
        self.emit(
            when(stg_n(ad, regs[0], n), P_TEMP_A, false),
            Eff::store(&r).preds(&[P_TEMP_A], &[]),
        );
    }

    // Control flow.

    fn if_(&mut self, cond: Reg, then_: &[Inst], else_: &[Inst]) -> Result<(), Error> {
        if let Loc::Pred(q) = self.loc(cond) {
            if self.predicable(then_) && self.predicable(else_) {
                let saved = self.guard;
                self.guard = Some((q, false));
                self.body(then_)?;
                self.guard = Some((q, true));
                self.body(else_)?;
                self.guard = saved;
                return Ok(());
            }
        }
        let (l_else, l_join) = (self.e.label(), self.e.label());
        let p = self.pred_of(cond);
        self.e.ctl(SSY, l_join, PT, false);
        self.e.ctl(BRA, l_else, p, true);
        self.body(then_)?;
        self.e.emit(sync(), &Eff::ctl(&[]));
        self.e.place(l_else);
        self.body(else_)?;
        self.e.emit(sync(), &Eff::ctl(&[]));
        self.e.place(l_join);
        Ok(())
    }

    /// A short body of arithmetic on numbers: every instruction can
    /// carry the guard.
    fn predicable(&self, b: &[Inst]) -> bool {
        self.guard.is_none() && crate::regalloc::short_alu(self.p, b)
    }

    fn loop_(&mut self, max_iter: u32, body: &[Inst]) -> Result<(), Error> {
        let c = self.al.loop_counters[self.loop_idx];
        self.loop_idx += 1;
        let (head, exit_l) = (self.e.label(), self.e.label());
        self.fx(mov32i(c, 0), &[c], &[]);
        self.e.ctl(PBK, exit_l, PT, false);
        self.e.place(head);
        self.e.ctl(PCNT, head, PT, false);
        self.emit(
            isetp_i(P_TEMP_B, c, max_iter as i32, Cmp::Ge, false),
            Eff::fixed(&[], &[c]).preds(&[], &[P_TEMP_B]),
        );
        self.e
            .emit(when(brk(), P_TEMP_B, false), &Eff::ctl(&[P_TEMP_B]));
        self.fx(iadd_i(c, c, 1), &[c], &[c]);
        self.body(body)?;
        self.e.emit(cont(), &Eff::ctl(&[]));
        self.e.place(exit_l);
        Ok(())
    }
}

fn icmp(op: CmpOp) -> Cmp {
    match op {
        CmpOp::Eq => Cmp::Eq,
        CmpOp::Ne => Cmp::Ne,
        CmpOp::Lt => Cmp::Lt,
        CmpOp::Le => Cmp::Le,
        CmpOp::Gt => Cmp::Gt,
        CmpOp::Ge => Cmp::Ge,
    }
}
