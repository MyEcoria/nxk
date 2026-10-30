//! Execution of the x87 instructions on the 80-bit soft-float core.
//!
//! State: eight 80-bit registers, TOP, the status word's flags and condition codes, the
//! control word and an "empty" bit per register. Exceptions are masked in practice; an
//! unmasked one stops before the result is written (the real x87 defers it to the next
//! waiting instruction and the hardware delivers bias-adjusted results: not modelled).

use super::{mask, Cpu, Mem, Stop, CF, FLAG_MASK, PF, ZF};
use crate::decode::{FSrc, Inst, Operand, X87Op};
use crate::fpu::{Cmp, F32, F64, IE};
use crate::fpu80::{self, Env80, X80};

/// FCW after `fninit` (and what the Linux host gives upstream's game: 64-bit precision,
/// round to nearest, all exceptions masked).
pub const FCW_RESET: u16 = 0x37f;
const SF: u32 = 0x40;
const C0: u16 = 1 << 8;
const C1: u16 = 1 << 9;
const C2: u16 = 1 << 10;
const C3: u16 = 1 << 14;

/// The x87 register file.
#[derive(Clone)]
pub struct Fpu {
    pub st: [X80; 8],
    /// Bit i set: physical register i is empty.
    pub empty: u8,
    pub top: u8,
    /// Status word without TOP.
    pub fsw: u16,
    pub fcw: u16,
}

impl Default for Fpu {
    fn default() -> Self {
        Fpu {
            st: [X80::ZERO; 8],
            empty: 0xff,
            top: 0,
            fsw: 0,
            fcw: FCW_RESET,
        }
    }
}

impl Fpu {
    /// The status word as `fnstsw` returns it.
    pub fn sw(&self) -> u16 {
        (self.fsw & !0x3800) | (self.top as u16) << 11
    }
    fn phys(&self, i: u8) -> usize {
        ((self.top + i) & 7) as usize
    }
}

fn size_of(s: FSrc) -> u8 {
    match s {
        FSrc::M32 | FSrc::I32 => 4,
        FSrc::M64 | FSrc::I64 => 8,
        FSrc::M80 => 10,
        FSrc::I16 => 2,
        FSrc::St(_) => 0,
    }
}

/// Value and fsw class of an x87 register for fxam / the tag word.
#[derive(PartialEq)]
enum Class {
    Unsupported,
    Nan,
    Normal,
    Inf,
    Zero,
    Denormal,
}

fn classify(v: X80) -> Class {
    let e = v.se & 0x7fff;
    if v.is_invalid_encoding() {
        Class::Unsupported
    } else if e == 0x7fff {
        if v.sig << 1 == 0 {
            Class::Inf
        } else {
            Class::Nan
        }
    } else if e == 0 {
        if v.sig == 0 {
            Class::Zero
        } else {
            Class::Denormal
        }
    } else {
        Class::Normal
    }
}

/// Constants of fld1 .. fldz as the host's x87 gives them (measured): (nearest, down, up).
/// Only fldl2t differs between nearest and up; its down value equals the nearest one.
const CONSTS: [[X80; 3]; 7] = {
    const fn k(se: u16, sig: u64) -> X80 {
        X80 { se, sig }
    }
    [
        [k(0x3fff, 1 << 63); 3],
        [
            k(0x4000, 0xd49a_784b_cd1b_8afe),
            k(0x4000, 0xd49a_784b_cd1b_8afe),
            k(0x4000, 0xd49a_784b_cd1b_8aff),
        ],
        [
            k(0x3fff, 0xb8aa_3b29_5c17_f0bc),
            k(0x3fff, 0xb8aa_3b29_5c17_f0bb),
            k(0x3fff, 0xb8aa_3b29_5c17_f0bc),
        ],
        [
            k(0x4000, 0xc90f_daa2_2168_c235),
            k(0x4000, 0xc90f_daa2_2168_c234),
            k(0x4000, 0xc90f_daa2_2168_c235),
        ],
        [
            k(0x3ffd, 0x9a20_9a84_fbcf_f799),
            k(0x3ffd, 0x9a20_9a84_fbcf_f798),
            k(0x3ffd, 0x9a20_9a84_fbcf_f799),
        ],
        [
            k(0x3ffe, 0xb172_17f7_d1cf_79ac),
            k(0x3ffe, 0xb172_17f7_d1cf_79ab),
            k(0x3ffe, 0xb172_17f7_d1cf_79ac),
        ],
        [X80::ZERO; 3],
    ]
};

impl Cpu {
    fn st_get(&self, i: u8, env: &mut Env80) -> X80 {
        let p = self.fpu.phys(i);
        if self.fpu.empty >> p & 1 != 0 {
            env.flags |= IE | SF;
            return X80::INDEFINITE;
        }
        self.fpu.st[p]
    }

    fn st_set(&mut self, i: u8, v: X80) {
        let p = self.fpu.phys(i);
        self.fpu.st[p] = v;
        self.fpu.empty &= !(1 << p);
    }

    fn fpush(&mut self, v: X80, env: &mut Env80) {
        self.fpu.top = self.fpu.top.wrapping_sub(1) & 7;
        let p = self.fpu.top as usize;
        let v = if self.fpu.empty >> p & 1 == 0 {
            env.flags |= IE | SF;
            env.c1 = true;
            X80::INDEFINITE
        } else {
            v
        };
        self.fpu.st[p] = v;
        self.fpu.empty &= !(1 << p);
    }

    fn fpop(&mut self) {
        let p = self.fpu.top as usize;
        self.fpu.empty |= 1 << p;
        self.fpu.top = (self.fpu.top + 1) & 7;
    }

    /// Fold an operation's flags and C1 into the status word; an unmasked exception stops.
    fn fraise(&mut self, env: &Env80, here: u64) -> Result<(), Stop> {
        self.fpu.fsw |= (env.flags & 0x7f) as u16;
        self.fpu.fsw = (self.fpu.fsw & !C1) | if env.c1 { C1 } else { 0 };
        if env.flags & 0x3f & !(self.fpu.fcw as u32) != 0 {
            return Err(Stop::FpException(here));
        }
        Ok(())
    }

    fn set_cc(&mut self, c: u16) {
        self.fpu.fsw = (self.fpu.fsw & !(C0 | C2 | C3)) | c;
    }

    fn cmp_cc(c: Cmp) -> u16 {
        match c {
            Cmp::Greater => 0,
            Cmp::Less => C0,
            Cmp::Equal => C3,
            Cmp::Unordered => C0 | C2 | C3,
        }
    }

    /// The second operand of an arithmetic or compare instruction.
    fn fload(
        &mut self,
        s: FSrc,
        i: &Inst,
        mem: &mut dyn Mem,
        next: u64,
        env: &mut Env80,
    ) -> Result<X80, Stop> {
        if let FSrc::St(n) = s {
            return Ok(self.st_get(n, env));
        }
        let Operand::Mem(m) = i.dst else {
            unreachable!("decoder: memory operand")
        };
        let a = self.addr(&m, next);
        let raw = self.read_n(a, size_of(s), mem)?;
        Ok(match s {
            FSrc::M32 => fpu80::from_float(F32, raw as u64, env),
            FSrc::M64 => fpu80::from_float(F64, raw as u64, env),
            FSrc::M80 => X80::from_bits(raw),
            FSrc::I16 => fpu80::from_int(raw as u16 as i16 as i64),
            FSrc::I32 => fpu80::from_int(raw as u32 as i32 as i64),
            FSrc::I64 => fpu80::from_int(raw as u64 as i64),
            FSrc::St(_) => unreachable!(),
        })
    }

    fn write_bytes(
        &mut self,
        i: &Inst,
        v: u128,
        n: u8,
        mem: &mut dyn Mem,
        next: u64,
    ) -> Result<(), Stop> {
        let Operand::Mem(m) = i.dst else {
            unreachable!("decoder: memory operand")
        };
        let a = self.addr(&m, next);
        let mut off = 0u8;
        while off < n {
            let step = (n - off).min(8);
            let at = a.wrapping_add(off as u64);
            if !mem.write(at, step, (v >> (8 * off as u32)) as u64 & mask(step)) {
                return Err(Stop::Fault(at));
            }
            off += step;
        }
        self.note_write(a, n);
        Ok(())
    }

    fn arith(r: u8, a: X80, b: X80, env: &mut Env80) -> X80 {
        match r {
            0 => fpu80::add(a, b, false, env),
            1 => fpu80::mul(a, b, env),
            4 => fpu80::add(a, b, true, env),
            5 => fpu80::add(b, a, true, env),
            6 => fpu80::div(a, b, env),
            _ => fpu80::div(b, a, env),
        }
    }

    pub(super) fn x87(
        &mut self,
        op: X87Op,
        i: Inst,
        mem: &mut dyn Mem,
        next: u64,
    ) -> Result<(), Stop> {
        let here = next.wrapping_sub(i.len as u64);
        let mut env = Env80::from_fcw(self.fpu.fcw);
        env.flags = 0;
        match op {
            X87Op::Nop => {}
            X87Op::Arith(r, s) => {
                let a = self.st_get(0, &mut env);
                let b = self.fload(s, &i, mem, next, &mut env)?;
                let v = Self::arith(r, a, b, &mut env);
                self.fraise(&env, here)?;
                self.st_set(0, v);
            }
            X87Op::ArithTo(r, n, pop) => {
                let (s0, sn) = (self.st_get(0, &mut env), self.st_get(n, &mut env));
                // DC / DE swap the sense of sub and div against D8.
                let r = match r {
                    4 => 5,
                    5 => 4,
                    6 => 7,
                    7 => 6,
                    r => r,
                };
                let v = Self::arith(r, sn, s0, &mut env);
                self.fraise(&env, here)?;
                self.st_set(n, v);
                if pop {
                    self.fpop();
                }
            }
            X87Op::Load(s) => {
                let v = self.fload(s, &i, mem, next, &mut env)?;
                self.fraise(&env, here)?;
                self.fpush(v, &mut env);
                self.fraise(&env, here)?;
            }
            X87Op::Const(k) => {
                use crate::fpu::Round;
                let v = CONSTS[k as usize][match env.round {
                    Round::Nearest => 0,
                    Round::Down | Round::Zero => 1,
                    Round::Up => 2,
                }];
                self.fpush(v, &mut env);
                self.fraise(&env, here)?;
            }
            X87Op::Store(s, pop) => {
                let a = self.st_get(0, &mut env);
                match s {
                    FSrc::St(n) => {
                        self.fraise(&env, here)?;
                        self.st_set(n, a);
                    }
                    FSrc::M32 | FSrc::M64 => {
                        let f = if s == FSrc::M32 { F32 } else { F64 };
                        let v = fpu80::to_float(f, a, &mut env);
                        self.fraise(&env, here)?;
                        self.write_bytes(&i, v as u128, size_of(s), mem, next)?;
                    }
                    FSrc::M80 => {
                        self.fraise(&env, here)?;
                        self.write_bytes(&i, a.to_bits(), 10, mem, next)?;
                    }
                    _ => {
                        let v = fpu80::to_int(a, 8 * size_of(s) as u32, false, &mut env);
                        self.fraise(&env, here)?;
                        self.write_bytes(&i, v as u128, size_of(s), mem, next)?;
                    }
                }
                if pop {
                    self.fpop();
                }
            }
            X87Op::Fisttp(s) => {
                let a = self.st_get(0, &mut env);
                let v = fpu80::to_int(a, 8 * size_of(s) as u32, true, &mut env);
                self.fraise(&env, here)?;
                self.write_bytes(&i, v as u128, size_of(s), mem, next)?;
                self.fpop();
            }
            X87Op::Xch(n) => {
                let (a, b) = (self.st_get(0, &mut env), self.st_get(n, &mut env));
                self.fraise(&env, here)?;
                self.st_set(0, b);
                self.st_set(n, a);
            }
            X87Op::Chs | X87Op::Abs => {
                let a = self.st_get(0, &mut env);
                self.fraise(&env, here)?;
                let se = if op == X87Op::Chs {
                    a.se ^ 0x8000
                } else {
                    a.se & 0x7fff
                };
                self.st_set(0, X80 { se, sig: a.sig });
            }
            X87Op::Tst | X87Op::Com(..) | X87Op::Comi(..) => {
                self.fcompare(op, i, mem, next, env, here)?
            }
            X87Op::Xam => {
                let p = self.fpu.phys(0);
                let v = self.fpu.st[p];
                let c = if self.fpu.empty >> p & 1 != 0 {
                    C3 | C0
                } else {
                    match classify(v) {
                        Class::Unsupported => 0,
                        Class::Nan => C0,
                        Class::Normal => C2,
                        Class::Inf => C2 | C0,
                        Class::Zero => C3,
                        Class::Denormal => C3 | C2,
                    }
                };
                self.set_cc(c);
                self.fpu.fsw = (self.fpu.fsw & !C1) | if v.sign() { C1 } else { 0 };
            }
            X87Op::Sqrt | X87Op::Rndint => {
                let a = self.st_get(0, &mut env);
                let v = if op == X87Op::Sqrt {
                    fpu80::sqrt(a, &mut env)
                } else {
                    fpu80::round_int(a, &mut env)
                };
                self.fraise(&env, here)?;
                self.st_set(0, v);
            }
            X87Op::Scale => {
                let (a, b) = (self.st_get(0, &mut env), self.st_get(1, &mut env));
                let v = fpu80::scale(a, b, &mut env);
                self.fraise(&env, here)?;
                self.st_set(0, v);
            }
            X87Op::Xtract => {
                let a = self.st_get(0, &mut env);
                let (e, s) = fpu80::xtract(a, &mut env);
                self.fraise(&env, here)?;
                self.st_set(0, e);
                self.fpush(s, &mut env);
                self.fraise(&env, here)?;
            }
            X87Op::Prem(nearest) => {
                let (a, b) = (self.st_get(0, &mut env), self.st_get(1, &mut env));
                let (v, c2, q) = fpu80::prem(a, b, nearest, &mut env);
                self.fraise(&env, here)?;
                self.st_set(0, v);
                let cc = if c2 {
                    C2
                } else {
                    (((q as u16 >> 2) & 1) * C0) | (((q as u16 >> 1) & 1) * C3)
                };
                self.set_cc(cc);
                self.fpu.fsw = (self.fpu.fsw & !C1) | if !c2 && q & 1 != 0 { C1 } else { 0 };
            }
            X87Op::Cmov(cc, n) => {
                let v = self.st_get(n, &mut env);
                let _ = self.st_get(0, &mut env);
                self.fraise(&env, here)?;
                if self.cond(cc) {
                    self.st_set(0, v);
                }
            }
            X87Op::Free(n) => {
                let p = self.fpu.phys(n);
                self.fpu.empty |= 1 << p;
            }
            X87Op::Incstp => self.fpu.top = (self.fpu.top + 1) & 7,
            X87Op::Decstp => self.fpu.top = self.fpu.top.wrapping_sub(1) & 7,
            X87Op::Init => self.fpu = Fpu::default(),
            X87Op::Clex => self.fpu.fsw &= !0x80ff,
            X87Op::Ldcw => {
                let Operand::Mem(m) = i.dst else {
                    unreachable!()
                };
                let a = self.addr(&m, next);
                self.fpu.fcw = mem.read(a, 2).ok_or(Stop::Fault(a))? as u16;
            }
            X87Op::Stcw => self.write_bytes(&i, self.fpu.fcw as u128, 2, mem, next)?,
            X87Op::StswMem => self.write_bytes(&i, self.fpu.sw() as u128, 2, mem, next)?,
            X87Op::StswAx => self.gpr[0] = (self.gpr[0] & !0xffff) | self.fpu.sw() as u64,
            X87Op::Stenv => {
                let mut tw = 0u16;
                for p in 0..8 {
                    let t = if self.fpu.empty >> p & 1 != 0 {
                        3
                    } else {
                        match classify(self.fpu.st[p]) {
                            Class::Zero => 1,
                            Class::Normal => 0,
                            _ => 2,
                        }
                    };
                    tw |= t << (2 * p);
                }
                let env28 = [
                    self.fpu.fcw as u32,
                    self.fpu.sw() as u32,
                    tw as u32,
                    0,
                    0,
                    0,
                    0,
                ];
                for (k, w) in env28.iter().enumerate() {
                    let Operand::Mem(m) = i.dst else {
                        unreachable!()
                    };
                    let a = self.addr(&m, next) + 4 * k as u64;
                    if !mem.write(a, 4, *w as u64) {
                        return Err(Stop::Fault(a));
                    }
                }
                // fnstenv masks every exception.
                self.fpu.fcw |= 0x3f;
            }
            X87Op::Ldenv => {
                let Operand::Mem(m) = i.dst else {
                    unreachable!()
                };
                let a = self.addr(&m, next);
                let rd = |mem: &mut dyn Mem, k: u64| {
                    mem.read(a + 4 * k, 4).ok_or(Stop::Fault(a + 4 * k))
                };
                self.fpu.fcw = rd(mem, 0)? as u16;
                let sw = rd(mem, 1)? as u16;
                let tw = rd(mem, 2)? as u16;
                self.fpu.top = (sw >> 11 & 7) as u8;
                self.fpu.fsw = sw & !0x3800;
                self.fpu.empty = (0..8).fold(0, |e, p| e | ((tw >> (2 * p) & 3 == 3) as u8) << p);
            }
        }
        Ok(())
    }

    /// Tst, Com and Comi: compare ST(0) with the operand.
    fn fcompare(
        &mut self,
        op: X87Op,
        i: Inst,
        mem: &mut dyn Mem,
        next: u64,
        mut env: Env80,
        here: u64,
    ) -> Result<(), Stop> {
        let a = self.st_get(0, &mut env);
        let (b, pops, unordered, eflags) = match op {
            X87Op::Tst => (X80::ZERO, 0, false, false),
            X87Op::Com(s, pops, un) => (self.fload(s, &i, mem, next, &mut env)?, pops, un, false),
            X87Op::Comi(n, pop, un) => (self.st_get(n, &mut env), pop as u8, un, true),
            _ => unreachable!(),
        };
        let c = fpu80::compare(a, b, !unordered, &mut env);
        // Comparisons that signal for an empty register give "unordered".
        let c = if env.flags & SF != 0 {
            Cmp::Unordered
        } else {
            c
        };
        self.fraise(&env, here)?;
        if eflags {
            let f = match c {
                Cmp::Unordered => ZF | PF | CF,
                Cmp::Less => CF,
                Cmp::Equal => ZF,
                Cmp::Greater => 0,
            };
            self.flags = (self.flags & !FLAG_MASK) | f;
            self.fpu.fsw &= !C1;
        } else {
            self.set_cc(Self::cmp_cc(c));
            self.fpu.fsw &= !C1;
        }
        for _ in 0..pops {
            self.fpop();
        }
        Ok(())
    }
}
