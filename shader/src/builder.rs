//! Writing a program: a register per result, nested closures for `if`
//! and loops. Types are checked as the program is written (a mistake
//! panics here); `validate` checks the finished program again, as the
//! service does with one it receives.

use alloc::vec::Vec;

use crate::ir::*;

pub struct Builder {
    stage: Stage,
    types: Vec<Ty>,
    stack: Vec<Vec<Inst>>,
    local_words: u16,
    local_size: [u16; 3],
}

macro_rules! bin {
    ($($(#[$m:meta])* $name:ident: $op:ident, $ta:ident;)*) => {$(
        $(#[$m])*
        pub fn $name(&mut self, a: Reg, b: Reg) -> Reg {
            self.expect(a, Ty::$ta);
            self.expect(b, Ty::$ta);
            let dst = self.reg(Ty::$ta);
            self.push(Inst::Bin { op: BinOp::$op, dst, a, b });
            dst
        }
    )*};
}

macro_rules! un {
    ($($(#[$m:meta])* $name:ident: $op:ident, $ta:ident -> $tr:ident;)*) => {$(
        $(#[$m])*
        pub fn $name(&mut self, a: Reg) -> Reg {
            self.expect(a, Ty::$ta);
            let dst = self.reg(Ty::$tr);
            self.push(Inst::Un { op: UnOp::$op, dst, a });
            dst
        }
    )*};
}

impl Builder {
    pub fn new(stage: Stage) -> Self {
        Self {
            stage,
            types: Vec::new(),
            stack: alloc::vec![Vec::new()],
            local_words: 0,
            local_size: [1, 1, 1],
        }
    }

    /// Compute: threads per group.
    pub fn local_size(&mut self, x: u16, y: u16, z: u16) {
        self.local_size = [x, y, z];
    }

    /// Scratch words per thread.
    pub fn local_words(&mut self, n: u16) {
        self.local_words = n;
    }

    pub fn ty(&self, r: Reg) -> Ty {
        self.types[r.0 as usize]
    }

    fn expect(&self, r: Reg, t: Ty) {
        assert!(
            self.ty(r) == t,
            "register {} is {:?}, expected {:?}",
            r.0,
            self.ty(r),
            t
        );
    }

    /// A register to assign with `mov` / in a branch / in a loop.
    pub fn reg(&mut self, t: Ty) -> Reg {
        assert!(self.types.len() < MAX_REGS, "too many registers");
        self.types.push(t);
        Reg((self.types.len() - 1) as u16)
    }

    fn push(&mut self, i: Inst) {
        self.stack.last_mut().unwrap().push(i);
    }

    pub fn finish(mut self) -> Program {
        assert!(self.stack.len() == 1);
        Program {
            stage: self.stage,
            reg_types: self.types,
            local_words: self.local_words,
            local_size: self.local_size,
            body: self.stack.pop().unwrap(),
        }
    }

    // Constants and moves.

    pub fn f32(&mut self, v: f32) -> Reg {
        let dst = self.reg(Ty::F32);
        self.push(Inst::Const {
            dst,
            bits: v.to_bits(),
        });
        dst
    }

    pub fn u32(&mut self, v: u32) -> Reg {
        let dst = self.reg(Ty::U32);
        self.push(Inst::Const { dst, bits: v });
        dst
    }

    pub fn i32(&mut self, v: i32) -> Reg {
        let dst = self.reg(Ty::I32);
        self.push(Inst::Const {
            dst,
            bits: v as u32,
        });
        dst
    }

    pub fn bool(&mut self, v: bool) -> Reg {
        let dst = self.reg(Ty::Bool);
        self.push(Inst::Const {
            dst,
            bits: v as u32,
        });
        dst
    }

    /// `dst = src`, for a register assigned more than once.
    pub fn mov(&mut self, dst: Reg, src: Reg) {
        assert!(self.ty(dst) == self.ty(src), "mov between types");
        self.push(Inst::Mov { dst, src });
    }

    /// A copy in a new register.
    pub fn copy(&mut self, src: Reg) -> Reg {
        let dst = self.reg(self.ty(src));
        self.push(Inst::Mov { dst, src });
        dst
    }

    // Arithmetic.

    bin! {
        fadd: FAdd, F32;
        fsub: FSub, F32;
        fmul: FMul, F32;
        fmin: FMin, F32;
        fmax: FMax, F32;
        iadd: IAdd, I32;
        isub: ISub, I32;
        imul: IMul, I32;
        imin: IMin, I32;
        imax: IMax, I32;
        uadd: IAdd, U32;
        usub: ISub, U32;
        umul: IMul, U32;
        umin: IMin, U32;
        umax: IMax, U32;
        and: And, U32;
        or: Or, U32;
        xor: Xor, U32;
        band: BAnd, Bool;
        bor: BOr, Bool;
        bxor: BXor, Bool;
    }

    un! {
        fneg: FNeg, F32 -> F32;
        fabs: FAbs, F32 -> F32;
        frcp: FRcp, F32 -> F32;
        frsq: FRsq, F32 -> F32;
        fsqrt: FSqrt, F32 -> F32;
        fsin: FSin, F32 -> F32;
        fcos: FCos, F32 -> F32;
        fex2: FEx2, F32 -> F32;
        flg2: FLg2, F32 -> F32;
        ffloor: FFloor, F32 -> F32;
        fsat: FSat, F32 -> F32;
        f2i: F2I, F32 -> I32;
        f2u: F2U, F32 -> U32;
        i2f: I2F, I32 -> F32;
        u2f: U2F, U32 -> F32;
        ineg: INeg, I32 -> I32;
        iabs: IAbs, I32 -> I32;
        not: Not, U32 -> U32;
        bnot: BNot, Bool -> Bool;
        /// The float's bits as a U32.
        fbits: FBits, F32 -> U32;
        /// A U32's bits as a float.
        bitsf: BitsF, U32 -> F32;
        ibits: IBits, U32 -> I32;
        ubits: UBits, I32 -> U32;
    }

    pub fn fma(&mut self, a: Reg, b: Reg, c: Reg) -> Reg {
        for r in [a, b, c] {
            self.expect(r, Ty::F32);
        }
        let dst = self.reg(Ty::F32);
        self.push(Inst::Fma { dst, a, b, c });
        dst
    }

    pub fn shl(&mut self, a: Reg, n: Reg) -> Reg {
        self.shift(BinOp::Shl, a, n, Ty::U32)
    }

    pub fn shr(&mut self, a: Reg, n: Reg) -> Reg {
        self.shift(BinOp::Shr, a, n, Ty::U32)
    }

    pub fn sar(&mut self, a: Reg, n: Reg) -> Reg {
        self.shift(BinOp::Sar, a, n, Ty::I32)
    }

    fn shift(&mut self, op: BinOp, a: Reg, n: Reg, t: Ty) -> Reg {
        self.expect(a, t);
        self.expect(n, Ty::U32);
        let dst = self.reg(t);
        self.push(Inst::Bin { op, dst, a, b: n });
        dst
    }

    /// p ? a : b.
    pub fn sel(&mut self, p: Reg, a: Reg, b: Reg) -> Reg {
        self.expect(p, Ty::Bool);
        assert!(self.ty(a) == self.ty(b));
        let dst = self.reg(self.ty(a));
        self.push(Inst::Sel { dst, p, a, b });
        dst
    }

    pub fn cmp(&mut self, op: CmpOp, a: Reg, b: Reg) -> Reg {
        assert!(self.ty(a) == self.ty(b) && self.ty(a) != Ty::Bool);
        let dst = self.reg(Ty::Bool);
        self.push(Inst::Cmp { op, dst, a, b });
        dst
    }

    // Inputs, outputs, memory.

    pub fn input(&mut self, attr: u8, comp: u8) -> Reg {
        self.input_as(attr, comp, Interp::Perspective)
    }

    pub fn input_as(&mut self, attr: u8, comp: u8, interp: Interp) -> Reg {
        let dst = self.reg(Ty::F32);
        self.push(Inst::Input {
            dst,
            attr,
            comp,
            interp,
        });
        dst
    }

    pub fn output(&mut self, kind: OutKind, comp: u8, src: Reg) {
        self.expect(src, Ty::F32);
        self.push(Inst::Output { kind, comp, src });
    }

    pub fn builtin(&mut self, which: Builtin) -> Reg {
        let t = match which {
            Builtin::FragX | Builtin::FragY | Builtin::FragZ => Ty::F32,
            _ => Ty::U32,
        };
        let dst = self.reg(t);
        self.push(Inst::Builtin { dst, which });
        dst
    }

    /// Word `word` of the uniform buffer, as type `t`.
    pub fn uniform(&mut self, word: u16, t: Ty) -> Reg {
        let dst = self.reg(t);
        self.push(Inst::Uniform { dst, word });
        dst
    }

    pub fn uniform_idx(&mut self, base: u16, index: Reg, t: Ty) -> Reg {
        self.expect(index, Ty::U32);
        let dst = self.reg(t);
        self.push(Inst::UniformIdx { dst, base, index });
        dst
    }

    /// One word of buffer `buf` at `index + offset`, as type `t`.
    pub fn load(&mut self, buf: u8, index: Reg, offset: u16, t: Ty) -> Reg {
        self.load_n(buf, index, offset, &[t])[0]
    }

    /// `types.len()` (1, 2 or 4) consecutive words.
    pub fn load_n(&mut self, buf: u8, index: Reg, offset: u16, types: &[Ty]) -> Vec<Reg> {
        self.expect(index, Ty::U32);
        let dsts: Vec<Reg> = types.iter().map(|&t| self.reg(t)).collect();
        self.push(Inst::BufLoad {
            buf,
            index,
            offset,
            dsts: dsts.clone(),
        });
        dsts
    }

    pub fn store(&mut self, buf: u8, index: Reg, offset: u16, srcs: &[Reg]) {
        self.expect(index, Ty::U32);
        self.push(Inst::BufStore {
            buf,
            index,
            offset,
            srcs: srcs.to_vec(),
        });
    }

    /// Adds `src` to a word of `buf`; the word it held.
    pub fn atomic_add(&mut self, buf: u8, index: Reg, src: Reg) -> Reg {
        self.expect(index, Ty::U32);
        self.expect(src, Ty::U32);
        let dst = self.reg(Ty::U32);
        self.push(Inst::BufAtomicAdd {
            dst,
            buf,
            index,
            src,
        });
        dst
    }

    pub fn local_load(&mut self, index: Reg, t: Ty) -> Reg {
        self.expect(index, Ty::U32);
        let dst = self.reg(t);
        self.push(Inst::LocalLoad { dst, index });
        dst
    }

    pub fn local_store(&mut self, index: Reg, src: Reg) {
        self.expect(index, Ty::U32);
        self.push(Inst::LocalStore { index, src });
    }

    /// RGBA of texture `tex` at (u, v), with derivatives (fragment only).
    pub fn sample(&mut self, tex: u8, u: Reg, v: Reg) -> [Reg; 4] {
        self.sample_at(tex, u, v, None)
    }

    pub fn sample_lod(&mut self, tex: u8, u: Reg, v: Reg, lod: Reg) -> [Reg; 4] {
        self.sample_at(tex, u, v, Some(lod))
    }

    fn sample_at(&mut self, tex: u8, u: Reg, v: Reg, lod: Option<Reg>) -> [Reg; 4] {
        for r in [u, v].into_iter().chain(lod) {
            self.expect(r, Ty::F32);
        }
        let dsts = [(); 4].map(|_| self.reg(Ty::F32));
        self.push(Inst::Tex {
            tex,
            coords: [u, v],
            lod,
            dsts,
        });
        dsts
    }

    // Control flow.

    fn block(&mut self, f: impl FnOnce(&mut Builder)) -> Vec<Inst> {
        self.stack.push(Vec::new());
        f(self);
        self.stack.pop().unwrap()
    }

    pub fn if_(&mut self, cond: Reg, then_: impl FnOnce(&mut Builder)) {
        self.if_else(cond, then_, |_| {});
    }

    pub fn if_else(
        &mut self,
        cond: Reg,
        then_: impl FnOnce(&mut Builder),
        else_: impl FnOnce(&mut Builder),
    ) {
        self.expect(cond, Ty::Bool);
        let then_ = self.block(then_);
        let else_ = self.block(else_);
        self.push(Inst::If { cond, then_, else_ });
    }

    /// At most `max_iter` turns of the body.
    pub fn loop_(&mut self, max_iter: u32, body: impl FnOnce(&mut Builder)) {
        let body = self.block(body);
        self.push(Inst::Loop { max_iter, body });
    }

    pub fn break_(&mut self) {
        self.push(Inst::Break { cond: None });
    }

    pub fn break_if(&mut self, cond: Reg) {
        self.expect(cond, Ty::Bool);
        self.push(Inst::Break { cond: Some(cond) });
    }

    pub fn continue_(&mut self) {
        self.push(Inst::Continue { cond: None });
    }

    pub fn continue_if(&mut self, cond: Reg) {
        self.expect(cond, Ty::Bool);
        self.push(Inst::Continue { cond: Some(cond) });
    }

    pub fn discard(&mut self) {
        self.push(Inst::Discard { cond: None });
    }

    pub fn discard_if(&mut self, cond: Reg) {
        self.expect(cond, Ty::Bool);
        self.push(Inst::Discard { cond: Some(cond) });
    }
}
