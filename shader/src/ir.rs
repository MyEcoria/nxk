//! The shader IR: typed virtual registers, structured control flow.
//! One program is one thread of a vertex, fragment or compute launch.
//! Every memory access goes through a bound buffer and is range-checked
//! (a load outside reads 0, a store outside is dropped) and every loop
//! has a trip limit, so the GPU service can run a program from a game
//! without trusting it.

use alloc::vec::Vec;

pub const MAX_REGS: usize = 192;
pub const MAX_INSTS: usize = 4096;
pub const MAX_DEPTH: usize = 8;
pub const MAX_BUFFERS: usize = 8;
pub const MAX_TEXTURES: usize = 16;
pub const MAX_UNIFORM_WORDS: usize = 1024;
pub const MAX_LOCAL_WORDS: usize = 128;
pub const MAX_VARYINGS: usize = 14;
pub const MAX_ATTRS: usize = 16;
pub const MAX_TARGETS: usize = 8;
pub const MAX_LOOP_ITERS: u32 = 0x7FFFF;

/// A virtual register; its type is `Program::reg_types[id]`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Reg(pub u16);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum Ty {
    F32 = 0,
    U32 = 1,
    I32 = 2,
    /// A predicate; has no bits to read back.
    Bool = 3,
}

impl Ty {
    pub fn from_u8(v: u8) -> Option<Ty> {
        Some(match v {
            0 => Ty::F32,
            1 => Ty::U32,
            2 => Ty::I32,
            3 => Ty::Bool,
            _ => return None,
        })
    }

    pub fn is_int(self) -> bool {
        matches!(self, Ty::U32 | Ty::I32)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Stage {
    Vertex = 0,
    Fragment = 1,
    Compute = 2,
}

impl Stage {
    pub fn from_u8(v: u8) -> Option<Stage> {
        Some(match v {
            0 => Stage::Vertex,
            1 => Stage::Fragment,
            2 => Stage::Compute,
            _ => return None,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum UnOp {
    /// F32 -> F32.
    FNeg = 0,
    FAbs,
    /// Approximate on hardware (MUFU), within a couple of ulps.
    FRcp,
    FRsq,
    FSqrt,
    FSin,
    FCos,
    FEx2,
    FLg2,
    FFloor,
    /// Clamp to 0..1.
    FSat,
    /// F32 -> I32 / U32, truncating and saturating; NaN is 0.
    F2I,
    F2U,
    /// I32 / U32 -> F32.
    I2F,
    U2F,
    /// I32 -> I32.
    INeg,
    IAbs,
    /// U32 -> U32, bitwise.
    Not,
    /// Bool -> Bool.
    BNot,
    /// Same bits, another type: F32 <-> U32 <-> I32.
    FBits,
    BitsF,
    IBits,
    UBits,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum BinOp {
    /// F32 x F32 -> F32.
    FAdd = 0,
    FSub,
    FMul,
    /// With a NaN operand, the other.
    FMin,
    FMax,
    /// U32 / I32 (same type both sides), wrapping.
    IAdd,
    ISub,
    /// Low 32 bits of the product.
    IMul,
    IMin,
    IMax,
    /// U32 x U32 -> U32, bitwise.
    And,
    Or,
    Xor,
    /// Shifts take the count (U32, second operand) modulo 32; `Shl` and
    /// `Shr` (logical) work on U32, `Sar` (arithmetic) on I32.
    Shl,
    Shr,
    Sar,
    /// Bool x Bool -> Bool.
    BAnd,
    BOr,
    BXor,
}

/// Ordered compare for floats, except `Ne`, true with a NaN (as C).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum CmpOp {
    Eq = 0,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Interp {
    /// Perspective-correct (the default).
    Perspective = 0,
    /// Screen-space linear.
    Linear,
    /// The provoking vertex's value.
    Flat,
}

/// What an output writes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum OutKind {
    /// Vertex: clip-space position, components 0-3.
    Position,
    /// Vertex: varying `n` (components 0-3), read by the fragment
    /// shader's input `n`.
    Varying(u8),
    /// Fragment: color target `n` (components 0-3 = RGBA).
    Color(u8),
    /// Fragment: depth, component 0.
    Depth,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Builtin {
    /// Compute: thread in the group, U32.
    LocalIdX = 0,
    LocalIdY,
    LocalIdZ,
    /// Compute: group in the launch, U32.
    GroupIdX,
    GroupIdY,
    GroupIdZ,
    /// Vertex: index of the vertex (or of its index-buffer entry), instance.
    VertexId,
    InstanceId,
    /// Fragment: window position of the pixel center (F32) and depth.
    FragX,
    FragY,
    FragZ,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Inst {
    Const {
        dst: Reg,
        bits: u32,
    },
    Mov {
        dst: Reg,
        src: Reg,
    },
    Un {
        op: UnOp,
        dst: Reg,
        a: Reg,
    },
    Bin {
        op: BinOp,
        dst: Reg,
        a: Reg,
        b: Reg,
    },
    /// a * b + c, fused.
    Fma {
        dst: Reg,
        a: Reg,
        b: Reg,
        c: Reg,
    },
    /// p ? a : b.
    Sel {
        dst: Reg,
        p: Reg,
        a: Reg,
        b: Reg,
    },
    /// Operands of one numeric type; the result is Bool.
    Cmp {
        op: CmpOp,
        dst: Reg,
        a: Reg,
        b: Reg,
    },
    /// Vertex: attribute `attr`'s component (as the vertex layout gives
    /// it, floats). Fragment: varying `attr`'s component.
    Input {
        dst: Reg,
        attr: u8,
        comp: u8,
        interp: Interp,
    },
    Output {
        kind: OutKind,
        comp: u8,
        src: Reg,
    },
    Builtin {
        dst: Reg,
        which: Builtin,
    },
    /// Word `word` of the launch's uniform buffer.
    Uniform {
        dst: Reg,
        word: u16,
    },
    /// Word `base + index` of the uniform buffer (index is U32).
    UniformIdx {
        dst: Reg,
        base: u16,
        index: Reg,
    },
    /// 1, 2 or 4 consecutive words of bound buffer `buf` from word
    /// `index + offset`; each `dsts[i]` takes the bits of word i.
    BufLoad {
        buf: u8,
        index: Reg,
        offset: u16,
        dsts: Vec<Reg>,
    },
    BufStore {
        buf: u8,
        index: Reg,
        offset: u16,
        srcs: Vec<Reg>,
    },
    /// dst = old word; the word += src (U32).
    BufAtomicAdd {
        dst: Reg,
        buf: u8,
        index: Reg,
        src: Reg,
    },
    /// The thread's own scratch words.
    LocalLoad {
        dst: Reg,
        index: Reg,
    },
    LocalStore {
        index: Reg,
        src: Reg,
    },
    /// Sample bound texture `tex` (normalized coordinates); `lod` is an
    /// explicit level (F32), none: derivatives (fragment shaders only).
    Tex {
        tex: u8,
        coords: [Reg; 2],
        lod: Option<Reg>,
        dsts: [Reg; 4],
    },
    If {
        cond: Reg,
        then_: Vec<Inst>,
        else_: Vec<Inst>,
    },
    /// Runs the body at most `max_iter` times: `Break` leaves, `Continue`
    /// goes to the next turn, and the loop ends after the last one.
    Loop {
        max_iter: u32,
        body: Vec<Inst>,
    },
    /// Leave / continue the innermost loop, when `cond` (Bool) holds.
    Break {
        cond: Option<Reg>,
    },
    Continue {
        cond: Option<Reg>,
    },
    /// Fragment: drop the pixel, when `cond` holds.
    Discard {
        cond: Option<Reg>,
    },
}

#[derive(Clone, Debug, PartialEq)]
pub struct Program {
    pub stage: Stage,
    pub reg_types: Vec<Ty>,
    /// Scratch words per thread (`LocalLoad` / `LocalStore`).
    pub local_words: u16,
    /// Compute: threads per group.
    pub local_size: [u16; 3],
    pub body: Vec<Inst>,
}

impl Program {
    pub fn ty(&self, r: Reg) -> Option<Ty> {
        self.reg_types.get(r.0 as usize).copied()
    }
}

/// Every instruction of a body, nested ones included, in order.
pub fn walk<'a>(body: &'a [Inst], f: &mut impl FnMut(&'a Inst)) {
    for i in body {
        f(i);
        match i {
            Inst::If { then_, else_, .. } => {
                walk(then_, f);
                walk(else_, f);
            }
            Inst::Loop { body, .. } => walk(body, f),
            _ => {}
        }
    }
}
