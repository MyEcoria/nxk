//! The reference machine: one thread of a validated program on the CPU.
//! The service checks the GPU's results against it (`gpu test`), and a
//! game can run its shaders on the CPU for a screenshot test. Memory
//! rules are the compiled code's: a vector access of n words needs an
//! index that is a multiple of n and room for all n, or a load gives 0
//! and a store does nothing.

use alloc::vec::Vec;

use crate::fmath;
use crate::ir::*;

/// Texture sampling for the reference machine.
pub trait Sampler {
    fn sample(&mut self, tex: u8, u: f32, v: f32, lod: Option<f32>) -> [f32; 4];
}

/// A sampler that returns black.
pub struct NoTextures;

impl Sampler for NoTextures {
    fn sample(&mut self, _: u8, _: f32, _: f32, _: Option<f32>) -> [f32; 4] {
        [0.0; 4]
    }
}

/// Everything one thread sees.
pub struct Machine<'a> {
    /// Vertex attributes / fragment varyings (bits), `[attr][comp]`.
    pub inputs: [[u32; 4]; MAX_ATTRS],
    /// Indexed by `Builtin as usize`.
    pub builtins: [u32; 11],
    pub uniforms: &'a [u32],
    pub buffers: &'a mut [Vec<u32>],
    pub sampler: &'a mut dyn Sampler,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Outputs {
    pub position: [f32; 4],
    pub varyings: [[f32; 4]; MAX_VARYINGS],
    pub colors: [[f32; 4]; MAX_TARGETS],
    pub depth: f32,
    pub discarded: bool,
}

impl Default for Outputs {
    fn default() -> Self {
        Self {
            position: [0.0; 4],
            varyings: [[0.0; 4]; MAX_VARYINGS],
            colors: [[0.0; 4]; MAX_TARGETS],
            depth: 0.0,
            discarded: false,
        }
    }
}

#[derive(PartialEq)]
enum Ctl {
    Next,
    Break,
    Continue,
    Kill,
}

struct Run<'a, 'm> {
    p: &'a Program,
    m: &'a mut Machine<'m>,
    regs: Vec<u32>,
    local: Vec<u32>,
    out: Outputs,
}

pub fn run(p: &Program, m: &mut Machine<'_>) -> Outputs {
    let mut r = Run {
        p,
        m,
        regs: alloc::vec![0; p.reg_types.len()],
        local: alloc::vec![0; p.local_words as usize],
        out: Outputs::default(),
    };
    r.body(&p.body);
    r.out
}

/// Relative error a test adds to every transcendental result, to see how
/// far the approximate hardware unit can move a program's outputs.
pub static MUFU_ERROR_BITS: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);

/// The range reduction of the sine and cosine is off in proportion to the
/// argument, not to the result.
fn approx_arg(x: f32) -> f32 {
    approx(x)
}

fn approx(v: f32) -> f32 {
    v * (1.0 + f32::from_bits(MUFU_ERROR_BITS.load(core::sync::atomic::Ordering::Relaxed)))
}

fn f(x: u32) -> f32 {
    f32::from_bits(x)
}

fn cmp<T: PartialOrd>(op: CmpOp, a: T, b: T) -> bool {
    match op {
        CmpOp::Eq => a == b,
        CmpOp::Ne => a != b,
        CmpOp::Lt => a < b,
        CmpOp::Le => a <= b,
        CmpOp::Gt => a > b,
        CmpOp::Ge => a >= b,
    }
}

/// Index of an n-word access in a buffer of `len` words, if it is
/// aligned and inside.
fn span(index: u32, offset: u16, n: usize, len: usize) -> Option<usize> {
    let i = index.wrapping_add(offset as u32) as usize;
    (i.is_multiple_of(n) && i < len && len - i >= n).then_some(i)
}

impl Run<'_, '_> {
    fn r(&self, r: Reg) -> u32 {
        self.regs[r.0 as usize]
    }

    fn set(&mut self, r: Reg, v: u32) {
        self.regs[r.0 as usize] = v;
    }

    fn rf(&self, r: Reg) -> f32 {
        f(self.r(r))
    }

    fn ty(&self, r: Reg) -> Ty {
        self.p.reg_types[r.0 as usize]
    }

    fn body(&mut self, b: &[Inst]) -> Ctl {
        for i in b {
            match self.inst(i) {
                Ctl::Next => {}
                c => return c,
            }
        }
        Ctl::Next
    }

    fn cond(&self, c: Option<Reg>) -> bool {
        c.is_none_or(|c| self.r(c) != 0)
    }

    fn inst(&mut self, i: &Inst) -> Ctl {
        match i {
            Inst::Const { dst, bits } => self.set(*dst, *bits),
            Inst::Mov { dst, src } => self.set(*dst, self.r(*src)),
            Inst::Un { op, dst, a } => {
                let v = self.un(*op, self.r(*a));
                self.set(*dst, v);
            }
            Inst::Bin { op, dst, a, b } => {
                let v = self.bin(*op, self.ty(*a), self.r(*a), self.r(*b));
                self.set(*dst, v);
            }
            Inst::Fma { dst, a, b, c } => {
                let v = (self.rf(*a) as f64 * self.rf(*b) as f64 + self.rf(*c) as f64) as f32;
                self.set(*dst, v.to_bits());
            }
            Inst::Sel { dst, p, a, b } => {
                let v = if self.r(*p) != 0 {
                    self.r(*a)
                } else {
                    self.r(*b)
                };
                self.set(*dst, v);
            }
            Inst::Cmp { op, dst, a, b } => {
                let (x, y) = (self.r(*a), self.r(*b));
                let v = match self.ty(*a) {
                    Ty::F32 => cmp(*op, f(x), f(y)),
                    Ty::I32 => cmp(*op, x as i32, y as i32),
                    _ => cmp(*op, x, y),
                };
                self.set(*dst, v as u32);
            }
            Inst::Input {
                dst, attr, comp, ..
            } => {
                let v = self.m.inputs[*attr as usize % MAX_ATTRS][*comp as usize % 4];
                self.set(*dst, v);
            }
            Inst::Output { kind, comp, src } => {
                let v = self.rf(*src);
                let c = *comp as usize % 4;
                match kind {
                    OutKind::Position => self.out.position[c] = v,
                    OutKind::Varying(n) => self.out.varyings[*n as usize % MAX_VARYINGS][c] = v,
                    OutKind::Color(n) => self.out.colors[*n as usize % MAX_TARGETS][c] = v,
                    OutKind::Depth => self.out.depth = v,
                }
            }
            Inst::Builtin { dst, which } => {
                let v = self.m.builtins[*which as usize];
                self.set(*dst, v);
            }
            Inst::Uniform { dst, word } => {
                let v = self.m.uniforms.get(*word as usize).copied().unwrap_or(0);
                self.set(*dst, v);
            }
            Inst::UniformIdx { dst, base, index } => {
                let i = (*base as u32).wrapping_add(self.r(*index)) as usize;
                let v = self.m.uniforms.get(i).copied().unwrap_or(0);
                self.set(*dst, v);
            }
            Inst::BufLoad {
                buf,
                index,
                offset,
                dsts,
            } => {
                let b = &self.m.buffers[*buf as usize];
                let at = span(self.r(*index), *offset, dsts.len(), b.len());
                for (k, d) in dsts.iter().enumerate() {
                    let v = at.map_or(0, |i| self.m.buffers[*buf as usize][i + k]);
                    self.set(*d, v);
                }
            }
            Inst::BufStore {
                buf,
                index,
                offset,
                srcs,
            } => {
                let len = self.m.buffers[*buf as usize].len();
                if let Some(i) = span(self.r(*index), *offset, srcs.len(), len) {
                    for (k, s) in srcs.iter().enumerate() {
                        let v = self.r(*s);
                        self.m.buffers[*buf as usize][i + k] = v;
                    }
                }
            }
            Inst::BufAtomicAdd {
                dst,
                buf,
                index,
                src,
            } => {
                let len = self.m.buffers[*buf as usize].len();
                let at = span(self.r(*index), 0, 1, len);
                let add = self.r(*src);
                let old = at.map_or(0, |i| {
                    let w = &mut self.m.buffers[*buf as usize][i];
                    let old = *w;
                    *w = old.wrapping_add(add);
                    old
                });
                self.set(*dst, old);
            }
            Inst::LocalLoad { dst, index } => {
                let v = self
                    .local
                    .get(self.r(*index) as usize)
                    .copied()
                    .unwrap_or(0);
                self.set(*dst, v);
            }
            Inst::LocalStore { index, src } => {
                let v = self.r(*src);
                if let Some(w) = self.local.get_mut(self.regs[index.0 as usize] as usize) {
                    *w = v;
                }
            }
            Inst::Tex {
                tex,
                coords,
                lod,
                dsts,
            } => {
                let (u, v) = (self.rf(coords[0]), self.rf(coords[1]));
                let l = lod.map(|l| self.rf(l));
                let t = self.m.sampler.sample(*tex, u, v, l);
                for (d, x) in dsts.iter().zip(t) {
                    self.set(*d, x.to_bits());
                }
            }
            Inst::If { cond, then_, else_ } => {
                let b = if self.r(*cond) != 0 { then_ } else { else_ };
                return self.body(b);
            }
            Inst::Loop { max_iter, body } => {
                for _ in 0..*max_iter {
                    match self.body(body) {
                        Ctl::Break => break,
                        Ctl::Kill => return Ctl::Kill,
                        _ => {}
                    }
                }
            }
            Inst::Break { cond } => {
                if self.cond(*cond) {
                    return Ctl::Break;
                }
            }
            Inst::Continue { cond } => {
                if self.cond(*cond) {
                    return Ctl::Continue;
                }
            }
            Inst::Discard { cond } => {
                if self.cond(*cond) {
                    self.out.discarded = true;
                    return Ctl::Kill;
                }
            }
        }
        Ctl::Next
    }

    fn un(&self, op: UnOp, a: u32) -> u32 {
        let x = f(a);
        let fl = |v: f32| v.to_bits();
        match op {
            UnOp::FNeg => a ^ 0x8000_0000,
            UnOp::FAbs => a & 0x7FFF_FFFF,
            UnOp::FRcp => fl(approx(fmath::rcp(x))),
            UnOp::FRsq => fl(approx(fmath::rsq(x))),
            UnOp::FSqrt => fl(approx(fmath::sqrt(x))),
            UnOp::FSin => fl(fmath::sin(approx_arg(x))),
            UnOp::FCos => fl(fmath::cos(approx_arg(x))),
            UnOp::FEx2 => fl(approx(fmath::ex2(x))),
            UnOp::FLg2 => fl(approx(fmath::lg2(x))),
            UnOp::FFloor => fl(fmath::floor(x)),
            // NaN gives 0 (clamp would keep it), as the hardware's .SAT.
            #[allow(clippy::manual_clamp)]
            UnOp::FSat => fl(x.max(0.0).min(1.0)),
            UnOp::F2I => (x as i32) as u32,
            UnOp::F2U => x as u32,
            UnOp::I2F => fl(a as i32 as f32),
            UnOp::U2F => fl(a as f32),
            UnOp::INeg => (a as i32).wrapping_neg() as u32,
            UnOp::IAbs => (a as i32).wrapping_abs() as u32,
            UnOp::Not => !a,
            UnOp::BNot => (a == 0) as u32,
            UnOp::FBits | UnOp::BitsF | UnOp::IBits | UnOp::UBits => a,
        }
    }

    fn bin(&self, op: BinOp, t: Ty, a: u32, b: u32) -> u32 {
        let (x, y) = (f(a), f(b));
        match op {
            BinOp::FAdd => (x + y).to_bits(),
            BinOp::FSub => (x - y).to_bits(),
            BinOp::FMul => (x * y).to_bits(),
            BinOp::FMin => x.min(y).to_bits(),
            BinOp::FMax => x.max(y).to_bits(),
            BinOp::IAdd => a.wrapping_add(b),
            BinOp::ISub => a.wrapping_sub(b),
            BinOp::IMul => a.wrapping_mul(b),
            BinOp::IMin => {
                if t == Ty::I32 {
                    (a as i32).min(b as i32) as u32
                } else {
                    a.min(b)
                }
            }
            BinOp::IMax => {
                if t == Ty::I32 {
                    (a as i32).max(b as i32) as u32
                } else {
                    a.max(b)
                }
            }
            BinOp::And | BinOp::BAnd => a & b,
            BinOp::Or | BinOp::BOr => a | b,
            BinOp::Xor | BinOp::BXor => a ^ b,
            BinOp::Shl => a << (b & 31),
            BinOp::Shr => a >> (b & 31),
            BinOp::Sar => ((a as i32) >> (b & 31)) as u32,
        }
    }
}
