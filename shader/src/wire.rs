//! A program as words (what a game puts in shared memory for the GPU
//! service). `decode` trusts nothing: every length is checked against
//! the words left, nesting is limited, and the result still goes
//! through `validate`.
//!
//! Layout: magic, `version | stage << 8`, `local_words`, group size x |
//! y << 16, z, register count, the register types (2 bits each, 16 per
//! word), then the body. An instruction is a head word (`op | sub << 8 |
//! n << 16`, n = the operand words that follow) and its operands; `If`
//! and `Loop` carry their bodies as operands.

use alloc::vec::Vec;

use crate::ir::*;

pub const MAGIC: u32 = 0x4853_584E; // "NXSH"
pub const VERSION: u32 = 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Error(pub &'static str);

impl core::fmt::Display for Error {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(self.0)
    }
}

const OP_CONST: u8 = 1;
const OP_MOV: u8 = 2;
const OP_UN: u8 = 3;
const OP_BIN: u8 = 4;
const OP_FMA: u8 = 5;
const OP_SEL: u8 = 6;
const OP_CMP: u8 = 7;
const OP_INPUT: u8 = 8;
const OP_OUTPUT: u8 = 9;
const OP_BUILTIN: u8 = 10;
const OP_UNIFORM: u8 = 11;
const OP_UNIFORM_IDX: u8 = 12;
const OP_BUF_LOAD: u8 = 13;
const OP_BUF_STORE: u8 = 14;
const OP_ATOMIC_ADD: u8 = 15;
const OP_LOCAL_LOAD: u8 = 16;
const OP_LOCAL_STORE: u8 = 17;
const OP_TEX: u8 = 18;
const OP_IF: u8 = 19;
const OP_LOOP: u8 = 20;
const OP_BREAK: u8 = 21;
const OP_CONTINUE: u8 = 22;
const OP_DISCARD: u8 = 23;

const NO_REG: u32 = 0xFFFF;

pub fn encode(p: &Program) -> Vec<u32> {
    let mut w = alloc::vec![
        MAGIC,
        VERSION | (p.stage as u32) << 8,
        p.local_words as u32,
        p.local_size[0] as u32 | (p.local_size[1] as u32) << 16,
        p.local_size[2] as u32,
        p.reg_types.len() as u32,
    ];
    for chunk in p.reg_types.chunks(16) {
        w.push(
            chunk
                .iter()
                .enumerate()
                .fold(0, |a, (i, &t)| a | (t as u32) << (2 * i)),
        );
    }
    body(&p.body, &mut w);
    w
}

fn body(b: &[Inst], w: &mut Vec<u32>) {
    for i in b {
        inst(i, w);
    }
}

fn r(x: Reg) -> u32 {
    x.0 as u32
}

fn opt(x: Option<Reg>) -> u32 {
    x.map_or(NO_REG, r)
}

fn put(w: &mut Vec<u32>, op: u8, sub: u8, operands: &[u32]) {
    w.push(op as u32 | (sub as u32) << 8 | (operands.len() as u32) << 16);
    w.extend_from_slice(operands);
}

fn inst(i: &Inst, w: &mut Vec<u32>) {
    match i {
        Inst::Const { dst, bits } => put(w, OP_CONST, 0, &[r(*dst), *bits]),
        Inst::Mov { dst, src } => put(w, OP_MOV, 0, &[r(*dst), r(*src)]),
        Inst::Un { op, dst, a } => put(w, OP_UN, *op as u8, &[r(*dst), r(*a)]),
        Inst::Bin { op, dst, a, b } => put(w, OP_BIN, *op as u8, &[r(*dst), r(*a), r(*b)]),
        Inst::Fma { dst, a, b, c } => put(w, OP_FMA, 0, &[r(*dst), r(*a), r(*b), r(*c)]),
        Inst::Sel { dst, p, a, b } => put(w, OP_SEL, 0, &[r(*dst), r(*p), r(*a), r(*b)]),
        Inst::Cmp { op, dst, a, b } => put(w, OP_CMP, *op as u8, &[r(*dst), r(*a), r(*b)]),
        Inst::Input {
            dst,
            attr,
            comp,
            interp,
        } => put(
            w,
            OP_INPUT,
            *interp as u8,
            &[r(*dst), *attr as u32 | (*comp as u32) << 8],
        ),
        Inst::Output { kind, comp, src } => {
            let (k, n) = match kind {
                OutKind::Position => (0, 0),
                OutKind::Varying(n) => (1, *n),
                OutKind::Color(n) => (2, *n),
                OutKind::Depth => (3, 0),
            };
            put(w, OP_OUTPUT, k, &[r(*src), n as u32 | (*comp as u32) << 8])
        }
        Inst::Builtin { dst, which } => put(w, OP_BUILTIN, *which as u8, &[r(*dst)]),
        Inst::Uniform { dst, word } => put(w, OP_UNIFORM, 0, &[r(*dst), *word as u32]),
        Inst::UniformIdx { dst, base, index } => {
            put(w, OP_UNIFORM_IDX, 0, &[r(*dst), *base as u32, r(*index)])
        }
        Inst::BufLoad {
            buf,
            index,
            offset,
            dsts,
        } => {
            let mut o = alloc::vec![*buf as u32 | (*offset as u32) << 8, r(*index)];
            o.extend(dsts.iter().map(|&d| r(d)));
            put(w, OP_BUF_LOAD, 0, &o);
        }
        Inst::BufStore {
            buf,
            index,
            offset,
            srcs,
        } => {
            let mut o = alloc::vec![*buf as u32 | (*offset as u32) << 8, r(*index)];
            o.extend(srcs.iter().map(|&d| r(d)));
            put(w, OP_BUF_STORE, 0, &o);
        }
        Inst::BufAtomicAdd {
            dst,
            buf,
            index,
            src,
        } => put(w, OP_ATOMIC_ADD, *buf, &[r(*dst), r(*index), r(*src)]),
        Inst::LocalLoad { dst, index } => put(w, OP_LOCAL_LOAD, 0, &[r(*dst), r(*index)]),
        Inst::LocalStore { index, src } => put(w, OP_LOCAL_STORE, 0, &[r(*index), r(*src)]),
        Inst::Tex {
            tex,
            coords,
            lod,
            dsts,
        } => put(
            w,
            OP_TEX,
            *tex,
            &[
                r(coords[0]),
                r(coords[1]),
                opt(*lod),
                r(dsts[0]),
                r(dsts[1]),
                r(dsts[2]),
                r(dsts[3]),
            ],
        ),
        Inst::If { cond, then_, else_ } => {
            let mut t = Vec::new();
            body(then_, &mut t);
            let mut e = Vec::new();
            body(else_, &mut e);
            let mut o = alloc::vec![r(*cond), t.len() as u32];
            o.extend(t);
            o.extend(e);
            put(w, OP_IF, 0, &o);
        }
        Inst::Loop { max_iter, body: b } => {
            let mut o = alloc::vec![*max_iter];
            body(b, &mut o);
            put(w, OP_LOOP, 0, &o);
        }
        Inst::Break { cond } => put(w, OP_BREAK, 0, &[opt(*cond)]),
        Inst::Continue { cond } => put(w, OP_CONTINUE, 0, &[opt(*cond)]),
        Inst::Discard { cond } => put(w, OP_DISCARD, 0, &[opt(*cond)]),
    }
}

type R<T> = Result<T, Error>;

fn err<T>(m: &'static str) -> R<T> {
    Err(Error(m))
}

pub fn decode(w: &[u32]) -> R<Program> {
    if w.len() < 6 || w[0] != MAGIC {
        return err("not a shader");
    }
    if w[1] & 0xFF != VERSION {
        return err("shader version");
    }
    let Some(stage) = Stage::from_u8((w[1] >> 8 & 0xFF) as u8) else {
        return err("stage");
    };
    if w[1] >> 16 != 0 || w[2] > u16::MAX as u32 || w[4] > u16::MAX as u32 {
        return err("header fields");
    }
    let n = w[5] as usize;
    if n > MAX_REGS {
        return err("too many registers");
    }
    let words = n.div_ceil(16);
    let Some(types) = w.get(6..6 + words) else {
        return err("truncated");
    };
    let mut reg_types = Vec::with_capacity(n);
    for i in 0..n {
        let t = (types[i / 16] >> (2 * (i % 16)) & 3) as u8;
        reg_types.push(Ty::from_u8(t).unwrap());
    }
    let b = decode_body(&w[6 + words..], 0)?;
    Ok(Program {
        stage,
        reg_types,
        local_words: w[2] as u16,
        local_size: [(w[3] & 0xFFFF) as u16, (w[3] >> 16) as u16, w[4] as u16],
        body: b,
    })
}

fn decode_body(mut w: &[u32], depth: usize) -> R<Vec<Inst>> {
    if depth > MAX_DEPTH + 1 {
        return err("nested too deep");
    }
    let mut out = Vec::new();
    while let Some(&head) = w.first() {
        let n = (head >> 16) as usize;
        let Some(o) = w.get(1..1 + n) else {
            return err("truncated instruction");
        };
        out.push(decode_inst(head, o, depth)?);
        w = &w[1 + n..];
        if out.len() > MAX_INSTS {
            return err("program too long");
        }
    }
    Ok(out)
}

fn reg(v: u32) -> R<Reg> {
    if v as usize >= MAX_REGS {
        return err("register out of range");
    }
    Ok(Reg(v as u16))
}

fn opt_reg(v: u32) -> R<Option<Reg>> {
    if v == NO_REG {
        Ok(None)
    } else {
        reg(v).map(Some)
    }
}

fn exact(o: &[u32], n: usize) -> R<()> {
    if o.len() != n {
        return err("operand count");
    }
    Ok(())
}

fn decode_inst(head: u32, o: &[u32], depth: usize) -> R<Inst> {
    let op = (head & 0xFF) as u8;
    let sub = (head >> 8 & 0xFF) as u8;
    Ok(match op {
        OP_CONST => {
            exact(o, 2)?;
            Inst::Const {
                dst: reg(o[0])?,
                bits: o[1],
            }
        }
        OP_MOV => {
            exact(o, 2)?;
            Inst::Mov {
                dst: reg(o[0])?,
                src: reg(o[1])?,
            }
        }
        OP_UN => {
            exact(o, 2)?;
            Inst::Un {
                op: un_op(sub)?,
                dst: reg(o[0])?,
                a: reg(o[1])?,
            }
        }
        OP_BIN => {
            exact(o, 3)?;
            Inst::Bin {
                op: bin_op(sub)?,
                dst: reg(o[0])?,
                a: reg(o[1])?,
                b: reg(o[2])?,
            }
        }
        OP_FMA => {
            exact(o, 4)?;
            Inst::Fma {
                dst: reg(o[0])?,
                a: reg(o[1])?,
                b: reg(o[2])?,
                c: reg(o[3])?,
            }
        }
        OP_SEL => {
            exact(o, 4)?;
            Inst::Sel {
                dst: reg(o[0])?,
                p: reg(o[1])?,
                a: reg(o[2])?,
                b: reg(o[3])?,
            }
        }
        OP_CMP => {
            exact(o, 3)?;
            Inst::Cmp {
                op: cmp_op(sub)?,
                dst: reg(o[0])?,
                a: reg(o[1])?,
                b: reg(o[2])?,
            }
        }
        OP_INPUT => {
            exact(o, 2)?;
            Inst::Input {
                dst: reg(o[0])?,
                attr: (o[1] & 0xFF) as u8,
                comp: (o[1] >> 8 & 0xFF) as u8,
                interp: match sub {
                    0 => Interp::Perspective,
                    1 => Interp::Linear,
                    2 => Interp::Flat,
                    _ => return err("interpolation"),
                },
            }
        }
        OP_OUTPUT => {
            exact(o, 2)?;
            let n = (o[1] & 0xFF) as u8;
            Inst::Output {
                kind: match sub {
                    0 => OutKind::Position,
                    1 => OutKind::Varying(n),
                    2 => OutKind::Color(n),
                    3 => OutKind::Depth,
                    _ => return err("output kind"),
                },
                comp: (o[1] >> 8 & 0xFF) as u8,
                src: reg(o[0])?,
            }
        }
        OP_BUILTIN => {
            exact(o, 1)?;
            Inst::Builtin {
                dst: reg(o[0])?,
                which: builtin(sub)?,
            }
        }
        OP_UNIFORM => {
            exact(o, 2)?;
            Inst::Uniform {
                dst: reg(o[0])?,
                word: u16::try_from(o[1]).or(err("uniform"))?,
            }
        }
        OP_UNIFORM_IDX => {
            exact(o, 3)?;
            Inst::UniformIdx {
                dst: reg(o[0])?,
                base: u16::try_from(o[1]).or(err("uniform"))?,
                index: reg(o[2])?,
            }
        }
        OP_BUF_LOAD | OP_BUF_STORE => {
            if o.len() < 3 || o.len() > 6 {
                return err("operand count");
            }
            let (buf, offset) = ((o[0] & 0xFF) as u8, (o[0] >> 8) as u16);
            if o[0] >> 24 != 0 {
                return err("offset");
            }
            let index = reg(o[1])?;
            let regs = o[2..].iter().map(|&v| reg(v)).collect::<R<Vec<_>>>()?;
            if op == OP_BUF_LOAD {
                Inst::BufLoad {
                    buf,
                    index,
                    offset,
                    dsts: regs,
                }
            } else {
                Inst::BufStore {
                    buf,
                    index,
                    offset,
                    srcs: regs,
                }
            }
        }
        OP_ATOMIC_ADD => {
            exact(o, 3)?;
            Inst::BufAtomicAdd {
                dst: reg(o[0])?,
                buf: sub,
                index: reg(o[1])?,
                src: reg(o[2])?,
            }
        }
        OP_LOCAL_LOAD => {
            exact(o, 2)?;
            Inst::LocalLoad {
                dst: reg(o[0])?,
                index: reg(o[1])?,
            }
        }
        OP_LOCAL_STORE => {
            exact(o, 2)?;
            Inst::LocalStore {
                index: reg(o[0])?,
                src: reg(o[1])?,
            }
        }
        OP_TEX => {
            exact(o, 7)?;
            Inst::Tex {
                tex: sub,
                coords: [reg(o[0])?, reg(o[1])?],
                lod: opt_reg(o[2])?,
                dsts: [reg(o[3])?, reg(o[4])?, reg(o[5])?, reg(o[6])?],
            }
        }
        OP_IF => {
            if o.len() < 2 {
                return err("operand count");
            }
            let t = o[1] as usize;
            if t > o.len() - 2 {
                return err("truncated body");
            }
            Inst::If {
                cond: reg(o[0])?,
                then_: decode_body(&o[2..2 + t], depth + 1)?,
                else_: decode_body(&o[2 + t..], depth + 1)?,
            }
        }
        OP_LOOP => {
            if o.is_empty() {
                return err("operand count");
            }
            Inst::Loop {
                max_iter: o[0],
                body: decode_body(&o[1..], depth + 1)?,
            }
        }
        OP_BREAK => {
            exact(o, 1)?;
            Inst::Break {
                cond: opt_reg(o[0])?,
            }
        }
        OP_CONTINUE => {
            exact(o, 1)?;
            Inst::Continue {
                cond: opt_reg(o[0])?,
            }
        }
        OP_DISCARD => {
            exact(o, 1)?;
            Inst::Discard {
                cond: opt_reg(o[0])?,
            }
        }
        _ => return err("opcode"),
    })
}

fn un_op(v: u8) -> R<UnOp> {
    use UnOp::*;
    const ALL: [UnOp; 22] = [
        FNeg, FAbs, FRcp, FRsq, FSqrt, FSin, FCos, FEx2, FLg2, FFloor, FSat, F2I, F2U, I2F, U2F,
        INeg, IAbs, Not, BNot, FBits, BitsF, IBits,
    ];
    if v == UBits as u8 {
        return Ok(UBits);
    }
    ALL.get(v as usize).copied().ok_or(Error("unary op"))
}

fn bin_op(v: u8) -> R<BinOp> {
    use BinOp::*;
    const ALL: [BinOp; 19] = [
        FAdd, FSub, FMul, FMin, FMax, IAdd, ISub, IMul, IMin, IMax, And, Or, Xor, Shl, Shr, Sar,
        BAnd, BOr, BXor,
    ];
    ALL.get(v as usize).copied().ok_or(Error("binary op"))
}

fn cmp_op(v: u8) -> R<CmpOp> {
    use CmpOp::*;
    [Eq, Ne, Lt, Le, Gt, Ge]
        .get(v as usize)
        .copied()
        .ok_or(Error("compare op"))
}

fn builtin(v: u8) -> R<Builtin> {
    use Builtin::*;
    [
        LocalIdX, LocalIdY, LocalIdZ, GroupIdX, GroupIdY, GroupIdZ, VertexId, InstanceId, FragX,
        FragY, FragZ,
    ]
    .get(v as usize)
    .copied()
    .ok_or(Error("builtin"))
}
