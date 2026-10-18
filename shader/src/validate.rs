//! Checking a program before it runs: types, ranges, stage rules, that
//! every register is assigned before it is read, and the interface it
//! declares (which inputs, outputs, buffers, textures it touches).

use alloc::vec::Vec;

use crate::ir::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Error(pub &'static str);

impl core::fmt::Display for Error {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(self.0)
    }
}

/// What a valid program touches.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Info {
    /// (attribute, component, interpolation), once each.
    pub inputs: Vec<(u8, u8, Interp)>,
    pub outputs: Vec<(OutKind, u8)>,
    /// Bit n: bound buffer n.
    pub buffers: u32,
    /// Bit n: bound texture n.
    pub textures: u32,
    /// Words of the uniform buffer read.
    pub uniform_words: u16,
    pub insts: usize,
    pub discards: bool,
    pub atomics: bool,
    /// Writes a buffer (store or atomic).
    pub stores: bool,
    pub loops: bool,
}

type R<T> = Result<T, Error>;

fn err<T>(m: &'static str) -> R<T> {
    Err(Error(m))
}

#[derive(Clone, Copy)]
struct Set([u64; 3]);

impl Set {
    fn has(&self, r: Reg) -> bool {
        self.0[r.0 as usize / 64] >> (r.0 % 64) & 1 != 0
    }

    fn add(&mut self, r: Reg) {
        self.0[r.0 as usize / 64] |= 1 << (r.0 % 64);
    }

    fn and(&mut self, o: &Set) {
        for i in 0..3 {
            self.0[i] &= o.0[i];
        }
    }
}

struct Ctx<'a> {
    p: &'a Program,
    info: Info,
}

#[derive(PartialEq)]
enum Flow {
    Open,
    /// Ends in an unconditional `Break` / `Continue`.
    Closed,
}

pub fn validate(p: &Program) -> R<Info> {
    if p.reg_types.len() > MAX_REGS {
        return err("too many registers");
    }
    if p.local_words as usize > MAX_LOCAL_WORDS {
        return err("too many local words");
    }
    if p.stage == Stage::Compute {
        let [x, y, z] = p.local_size.map(|v| v as u32);
        if x == 0 || y == 0 || z == 0 || x * y * z > 1024 {
            return err("bad group size");
        }
    }
    let mut c = Ctx {
        p,
        info: Info::default(),
    };
    let mut defs = Set([0; 3]);
    c.body(&p.body, &mut defs, 0, 0)?;
    if c.info.insts > MAX_INSTS {
        return err("program too long");
    }
    Ok(c.info)
}

impl Ctx<'_> {
    fn ty(&self, r: Reg) -> R<Ty> {
        match self.p.ty(r) {
            Some(t) => Ok(t),
            None => err("register out of range"),
        }
    }

    fn use_(&self, r: Reg, defs: &Set) -> R<Ty> {
        let t = self.ty(r)?;
        if !defs.has(r) {
            return err("register read before it is assigned");
        }
        Ok(t)
    }

    fn use_as(&self, r: Reg, t: Ty, defs: &Set) -> R<()> {
        if self.use_(r, defs)? != t {
            return err("operand type");
        }
        Ok(())
    }

    fn def(&self, r: Reg, t: Ty, defs: &mut Set) -> R<()> {
        if self.ty(r)? != t {
            return err("destination type");
        }
        defs.add(r);
        Ok(())
    }

    fn body(&mut self, b: &[Inst], defs: &mut Set, depth: usize, loops: usize) -> R<Flow> {
        if depth > MAX_DEPTH {
            return err("nested too deep");
        }
        let mut flow = Flow::Open;
        for i in b {
            if flow == Flow::Closed {
                return err("code after a break or continue");
            }
            self.info.insts += 1;
            if self.info.insts > MAX_INSTS {
                return err("program too long");
            }
            flow = self.inst(i, defs, depth, loops)?;
        }
        Ok(flow)
    }

    fn inst(&mut self, i: &Inst, defs: &mut Set, depth: usize, loops: usize) -> R<Flow> {
        let stage = self.p.stage;
        match i {
            Inst::Const { dst, bits } => {
                if self.ty(*dst)? == Ty::Bool && *bits > 1 {
                    return err("bool constant");
                }
                defs.add(*dst);
            }
            Inst::Mov { dst, src } => {
                let t = self.use_(*src, defs)?;
                self.def(*dst, t, defs)?;
            }
            Inst::Un { op, dst, a } => {
                let (ta, tr) = un_types(*op);
                self.use_as(*a, ta, defs)?;
                self.def(*dst, tr, defs)?;
            }
            Inst::Bin { op, dst, a, b } => {
                let ta = self.use_(*a, defs)?;
                let tb = self.use_(*b, defs)?;
                let ok = match op {
                    BinOp::FAdd | BinOp::FSub | BinOp::FMul | BinOp::FMin | BinOp::FMax => {
                        ta == Ty::F32 && tb == Ty::F32
                    }
                    BinOp::IAdd | BinOp::ISub | BinOp::IMul | BinOp::IMin | BinOp::IMax => {
                        ta.is_int() && ta == tb
                    }
                    BinOp::And | BinOp::Or | BinOp::Xor => ta == Ty::U32 && tb == Ty::U32,
                    BinOp::Shl | BinOp::Shr => ta == Ty::U32 && tb == Ty::U32,
                    BinOp::Sar => ta == Ty::I32 && tb == Ty::U32,
                    BinOp::BAnd | BinOp::BOr | BinOp::BXor => ta == Ty::Bool && tb == Ty::Bool,
                };
                if !ok {
                    return err("operand types");
                }
                self.def(*dst, ta, defs)?;
            }
            Inst::Fma { dst, a, b, c } => {
                for r in [a, b, c] {
                    self.use_as(*r, Ty::F32, defs)?;
                }
                self.def(*dst, Ty::F32, defs)?;
            }
            Inst::Sel { dst, p, a, b } => {
                self.use_as(*p, Ty::Bool, defs)?;
                let t = self.use_(*a, defs)?;
                self.use_as(*b, t, defs)?;
                self.def(*dst, t, defs)?;
            }
            Inst::Cmp { dst, a, b, .. } => {
                let t = self.use_(*a, defs)?;
                if t == Ty::Bool {
                    return err("compare of predicates");
                }
                self.use_as(*b, t, defs)?;
                self.def(*dst, Ty::Bool, defs)?;
            }
            Inst::Input {
                dst,
                attr,
                comp,
                interp,
            } => {
                let max = match stage {
                    Stage::Vertex => MAX_ATTRS,
                    Stage::Fragment => MAX_VARYINGS,
                    Stage::Compute => return err("input in a compute shader"),
                };
                if *attr as usize >= max || *comp >= 4 {
                    return err("input out of range");
                }
                if stage == Stage::Vertex && *interp != Interp::Perspective {
                    return err("interpolation on a vertex input");
                }
                let k = (*attr, *comp, *interp);
                if self
                    .info
                    .inputs
                    .iter()
                    .any(|&(a, c, _)| (a, c) == (k.0, k.1))
                {
                    if !self.info.inputs.contains(&k) {
                        return err("one input read with two interpolations");
                    }
                } else {
                    self.info.inputs.push(k);
                }
                self.def(*dst, Ty::F32, defs)?;
            }
            Inst::Output { kind, comp, src } => {
                let ok = match (stage, kind) {
                    (Stage::Vertex, OutKind::Position) => *comp < 4,
                    (Stage::Vertex, OutKind::Varying(n)) => {
                        (*n as usize) < MAX_VARYINGS && *comp < 4
                    }
                    (Stage::Fragment, OutKind::Color(n)) => {
                        (*n as usize) < MAX_TARGETS && *comp < 4
                    }
                    (Stage::Fragment, OutKind::Depth) => *comp == 0,
                    _ => false,
                };
                if !ok {
                    return err("output not allowed here");
                }
                self.use_as(*src, Ty::F32, defs)?;
                if !self.info.outputs.contains(&(*kind, *comp)) {
                    self.info.outputs.push((*kind, *comp));
                }
            }
            Inst::Builtin { dst, which } => {
                let (ok, t) = match which {
                    Builtin::LocalIdX
                    | Builtin::LocalIdY
                    | Builtin::LocalIdZ
                    | Builtin::GroupIdX
                    | Builtin::GroupIdY
                    | Builtin::GroupIdZ => (stage == Stage::Compute, Ty::U32),
                    Builtin::VertexId | Builtin::InstanceId => (stage == Stage::Vertex, Ty::U32),
                    Builtin::FragX | Builtin::FragY | Builtin::FragZ => {
                        (stage == Stage::Fragment, Ty::F32)
                    }
                };
                if !ok {
                    return err("builtin not available in this stage");
                }
                self.def(*dst, t, defs)?;
            }
            Inst::Uniform { dst, word } => {
                if *word as usize >= MAX_UNIFORM_WORDS {
                    return err("uniform out of range");
                }
                if self.ty(*dst)? == Ty::Bool {
                    return err("uniform into a predicate");
                }
                self.info.uniform_words = self.info.uniform_words.max(*word + 1);
                defs.add(*dst);
            }
            Inst::UniformIdx { dst, base, index } => {
                if *base as usize >= MAX_UNIFORM_WORDS {
                    return err("uniform out of range");
                }
                self.use_as(*index, Ty::U32, defs)?;
                if self.ty(*dst)? == Ty::Bool {
                    return err("uniform into a predicate");
                }
                // The index is not known: the whole buffer may be read.
                self.info.uniform_words = MAX_UNIFORM_WORDS as u16;
                defs.add(*dst);
            }
            Inst::BufLoad {
                buf, index, dsts, ..
            } => {
                self.buffer(*buf)?;
                if !matches!(dsts.len(), 1 | 2 | 4) {
                    return err("load width");
                }
                self.use_as(*index, Ty::U32, defs)?;
                for d in dsts {
                    if self.ty(*d)? == Ty::Bool {
                        return err("load into a predicate");
                    }
                    defs.add(*d);
                }
            }
            Inst::BufStore {
                buf, index, srcs, ..
            } => {
                self.buffer(*buf)?;
                if !matches!(srcs.len(), 1 | 2 | 4) {
                    return err("store width");
                }
                self.use_as(*index, Ty::U32, defs)?;
                for s in srcs {
                    if self.use_(*s, defs)? == Ty::Bool {
                        return err("store of a predicate");
                    }
                }
                self.info.stores = true;
            }
            Inst::BufAtomicAdd {
                dst,
                buf,
                index,
                src,
            } => {
                self.buffer(*buf)?;
                self.use_as(*index, Ty::U32, defs)?;
                self.use_as(*src, Ty::U32, defs)?;
                self.def(*dst, Ty::U32, defs)?;
                self.info.atomics = true;
                self.info.stores = true;
            }
            Inst::LocalLoad { dst, index } => {
                if self.p.local_words == 0 {
                    return err("no local words declared");
                }
                self.use_as(*index, Ty::U32, defs)?;
                if self.ty(*dst)? == Ty::Bool {
                    return err("load into a predicate");
                }
                defs.add(*dst);
            }
            Inst::LocalStore { index, src } => {
                if self.p.local_words == 0 {
                    return err("no local words declared");
                }
                self.use_as(*index, Ty::U32, defs)?;
                if self.use_(*src, defs)? == Ty::Bool {
                    return err("store of a predicate");
                }
            }
            Inst::Tex {
                tex,
                coords,
                lod,
                dsts,
            } => {
                if *tex as usize >= MAX_TEXTURES {
                    return err("texture out of range");
                }
                self.info.textures |= 1 << tex;
                for r in coords.iter().chain(lod.iter()) {
                    self.use_as(*r, Ty::F32, defs)?;
                }
                if lod.is_none() && stage != Stage::Fragment {
                    return err("sampling without a level outside a fragment shader");
                }
                for d in dsts {
                    self.def(*d, Ty::F32, defs)?;
                }
            }
            Inst::If { cond, then_, else_ } => {
                self.use_as(*cond, Ty::Bool, defs)?;
                let mut dt = *defs;
                let mut de = *defs;
                let ft = self.body(then_, &mut dt, depth + 1, loops)?;
                let fe = self.body(else_, &mut de, depth + 1, loops)?;
                match (ft, fe) {
                    (Flow::Closed, Flow::Closed) => return Ok(Flow::Closed),
                    (Flow::Closed, _) => *defs = de,
                    (_, Flow::Closed) => *defs = dt,
                    _ => {
                        dt.and(&de);
                        *defs = dt;
                    }
                }
            }
            Inst::Loop { max_iter, body } => {
                if *max_iter == 0 || *max_iter > MAX_LOOP_ITERS {
                    return err("loop limit");
                }
                self.info.loops = true;
                let mut d = *defs;
                self.body(body, &mut d, depth + 1, loops + 1)?;
            }
            Inst::Break { cond } | Inst::Continue { cond } => {
                if loops == 0 {
                    return err("break or continue outside a loop");
                }
                return match cond {
                    Some(c) => {
                        self.use_as(*c, Ty::Bool, defs)?;
                        Ok(Flow::Open)
                    }
                    None => Ok(Flow::Closed),
                };
            }
            Inst::Discard { cond } => {
                if stage != Stage::Fragment {
                    return err("discard outside a fragment shader");
                }
                if let Some(c) = cond {
                    self.use_as(*c, Ty::Bool, defs)?;
                }
                self.info.discards = true;
            }
        }
        Ok(Flow::Open)
    }

    fn buffer(&mut self, buf: u8) -> R<()> {
        if buf as usize >= MAX_BUFFERS {
            return err("buffer out of range");
        }
        self.info.buffers |= 1 << buf;
        Ok(())
    }
}

/// (operand, result) of a unary operation.
pub fn un_types(op: UnOp) -> (Ty, Ty) {
    use Ty::*;
    use UnOp::*;
    match op {
        FNeg | FAbs | FRcp | FRsq | FSqrt | FSin | FCos | FEx2 | FLg2 | FFloor | FSat => (F32, F32),
        F2I => (F32, I32),
        F2U => (F32, U32),
        I2F => (I32, F32),
        U2F => (U32, F32),
        INeg | IAbs => (I32, I32),
        Not => (U32, U32),
        BNot => (Bool, Bool),
        FBits => (F32, U32),
        BitsF => (U32, F32),
        IBits => (U32, I32),
        UBits => (I32, U32),
    }
}
