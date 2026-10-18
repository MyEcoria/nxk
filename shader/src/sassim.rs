//! A one-thread simulator of the SASS the compiler emits, for tests:
//! it executes the code, and it checks what the hardware would punish,
//! that nothing reads a register before its producer is done (fixed
//! latencies, scoreboards waited on), writes one still being read, or
//! leaves the convergence stack unbalanced. The emulator runs no
//! shaders, so this is how the compiler is tested off the console.

use std::collections::BTreeMap;
use std::format;
use std::string::String;
use std::vec::Vec;

use crate::interp::Sampler;

const VAR_LATENCY: u64 = 30;
const GPR_LATENCY: u64 = 6;
const PRED_LATENCY: u64 = 13;
const READ_WINDOW: u64 = 4;

pub struct Sim<'a> {
    pub code: &'a [u64],
    /// Constant buffers 0-3 (words).
    pub cbuf: [Vec<u32>; 4],
    pub attrs_in: BTreeMap<u32, u32>,
    pub attrs_out: BTreeMap<u32, u32>,
    pub mem: Vec<(u64, Vec<u32>)>,
    pub sampler: &'a mut dyn Sampler,
    /// S2R values by special register number.
    pub sregs: BTreeMap<u8, u32>,
    pub regs: [u32; 256],
    pub killed: bool,
    pub steps: u64,
    cur_addr: core::cell::Cell<i64>,
}

#[derive(Clone, Copy, PartialEq)]
enum Tok {
    Sync,
    Brk,
    Cont,
}

fn bits(w: u64, lo: u32, n: u32) -> u64 {
    (w >> lo) & ((1u64 << n) - 1)
}

fn f(x: u32) -> f32 {
    f32::from_bits(x)
}

struct Timing {
    cycle: u64,
    ready: [u64; 256],
    pready: [u64; 8],
    pend_w: [Option<u8>; 256],
    pend_r: [Option<u8>; 256],
    read_until: [u64; 256],
    bar_done: [u64; 6],
}

impl<'a> Sim<'a> {
    pub fn new(code: &'a [u64], sampler: &'a mut dyn Sampler) -> Self {
        Self {
            code,
            cbuf: [Vec::new(), Vec::new(), Vec::new(), Vec::new()],
            attrs_in: BTreeMap::new(),
            attrs_out: BTreeMap::new(),
            mem: Vec::new(),
            sampler,
            sregs: BTreeMap::new(),
            regs: [0; 256],
            killed: false,
            steps: 0,
            cur_addr: core::cell::Cell::new(0),
        }
    }

    fn word(&self, i: usize) -> (u64, u64) {
        // (instruction, scheduling word)
        let g = i / 3;
        (self.code[g * 4 + 1 + i % 3], self.code[g * 4])
    }

    fn cb(&self, idx: usize, byte: u64) -> u32 {
        self.cbuf[idx]
            .get(byte as usize / 4)
            .copied()
            .unwrap_or(0xDEAD_C0DE)
    }

    fn mem_ptr(&mut self, addr: u64, words: usize) -> Option<&mut [u32]> {
        for (base, v) in self.mem.iter_mut() {
            if addr >= *base && addr + 4 * words as u64 <= *base + 4 * v.len() as u64 {
                let i = ((addr - *base) / 4) as usize;
                return Some(&mut v[i..i + words]);
            }
        }
        None
    }

    pub fn run(&mut self) -> Result<(), String> {
        let n = self.code.len() / 4 * 3;
        let mut pc = 0usize;
        let mut preds = [false; 8];
        preds[7] = true;
        let mut cc = false;
        let mut stack: Vec<(Tok, usize)> = Vec::new();
        let mut t = Timing {
            cycle: 0,
            ready: [0; 256],
            pready: [0; 8],
            pend_w: [None; 256],
            pend_r: [None; 256],
            read_until: [0; 256],
            bar_done: [0; 6],
        };
        loop {
            self.steps += 1;
            if self.steps > 2_000_000 {
                return Err("did not stop".into());
            }
            if pc >= n {
                return Err("ran off the end".into());
            }
            let (w, sched) = self.word(pc);
            let sched = sched >> (21 * (pc % 3)) & 0x1F_FFFF;
            let stall = sched & 0xF;
            let wr = (sched >> 5 & 7) as u8;
            let rd = (sched >> 8 & 7) as u8;
            let wait = (sched >> 11 & 0x3F) as u8;
            let op = (w >> 48) as u16;
            let guard = bits(w, 16, 3) as usize;
            let gnot = bits(w, 19, 1) == 1;
            let at = |s: &str| format!("pc {pc} word {w:016x}: {s}");

            // Waits first.
            for b in 0..6 {
                if wait >> b & 1 == 1 {
                    t.cycle = t.cycle.max(t.bar_done[b]);
                    for r in 0..256 {
                        if t.pend_w[r] == Some(b as u8) {
                            t.pend_w[r] = None;
                        }
                        if t.pend_r[r] == Some(b as u8) {
                            t.pend_r[r] = None;
                        }
                    }
                }
            }
            if (wr != 7 || rd != 7) && stall < 2 {
                return Err(at("a barrier is set with a stall under 2"));
            }
            let mut preads: Vec<usize> = Vec::new();
            if guard != 7 {
                preads.push(guard);
            }
            let active = preds[guard] != gnot;

            let e = self.effects(w, op)?;
            let reads = e.reads;
            let writes = e.writes;
            preads.extend(e.preads);
            let pwrites = e.pwrites;
            let (var, store, ctl) = (e.var, e.store, e.ctl);

            // Hazards.
            for &r in reads.iter().filter(|&&r| r != 255) {
                if t.pend_w[r].is_some() {
                    return Err(at(&format!("R{r} read before its load is waited on")));
                }
                if t.ready[r] > t.cycle {
                    return Err(at(&format!(
                        "R{r} read {} cycles early",
                        t.ready[r] - t.cycle
                    )));
                }
            }
            for &r in writes.iter().filter(|&&r| r != 255) {
                if t.pend_w[r].is_some() || t.pend_r[r].is_some() {
                    return Err(at(&format!("R{r} written while a load or store uses it")));
                }
                if t.read_until[r] > t.cycle {
                    return Err(at(&format!("R{r} written while still being read")));
                }
            }
            for &p in preads.iter().filter(|&&p| p != 7) {
                if t.pready[p] > t.cycle {
                    return Err(at(&format!(
                        "P{p} read {} cycles early",
                        t.pready[p] - t.cycle
                    )));
                }
            }
            // Timing after the instruction.
            let bar_for = |x: u8| if x == 7 { None } else { Some(x) };
            if var || store {
                if let Some(bw) = bar_for(wr) {
                    t.bar_done[bw as usize] = t.cycle + VAR_LATENCY;
                    for &r in writes.iter().filter(|&&r| r != 255) {
                        t.pend_w[r] = Some(bw);
                    }
                    for &r in &reads {
                        t.read_until[r] = t.cycle + READ_WINDOW;
                    }
                } else if let Some(br) = bar_for(rd) {
                    t.bar_done[br as usize] = t.cycle + VAR_LATENCY;
                    for &r in reads.iter().filter(|&&r| r != 255) {
                        t.pend_r[r] = Some(br);
                    }
                } else if !writes.is_empty() {
                    return Err(at("a variable-latency result without a scoreboard"));
                } else if store {
                    return Err(at("a store without a read barrier"));
                }
            } else {
                for &r in writes.iter().filter(|&&r| r != 255) {
                    t.ready[r] = t.cycle + GPR_LATENCY;
                }
            }
            for &p in &pwrites {
                t.pready[p] = t.cycle + PRED_LATENCY;
            }
            let issue = t.cycle;
            t.cycle += stall.max(1);

            // Execute.
            let mut next = pc + 1;
            self.cur_addr
                .set(pc as i64 / 3 * 32 + 8 + (pc as i64 % 3) * 8);
            if active {
                match self.exec(w, op, &mut preds, &mut cc)? {
                    Flow::Next => {}
                    Flow::Exit => {
                        if !stack.is_empty() {
                            return Err(at("exit with tokens on the convergence stack"));
                        }
                        return Ok(());
                    }
                    Flow::Jump(addr) => next = index_of(addr),
                    Flow::Ssy(addr) => stack.push((Tok::Sync, index_of(addr))),
                    Flow::Pbk(addr) => stack.push((Tok::Brk, index_of(addr))),
                    Flow::Pcnt(addr) => stack.push((Tok::Cont, index_of(addr))),
                    Flow::Sync => match stack.pop() {
                        Some((Tok::Sync, to)) => next = to,
                        _ => return Err(at("SYNC without a sync token on top")),
                    },
                    Flow::Brk => loop {
                        match stack.pop() {
                            Some((Tok::Brk, to)) => {
                                next = to;
                                break;
                            }
                            Some(_) => {}
                            None => return Err(at("BRK outside a loop")),
                        }
                    },
                    Flow::Cont => loop {
                        match stack.pop() {
                            Some((Tok::Cont, to)) => {
                                next = to;
                                break;
                            }
                            Some((Tok::Sync, _)) => {}
                            Some((Tok::Brk, _)) => return Err(at("CONT outside a loop")),
                            None => return Err(at("CONT outside a loop")),
                        }
                    },
                    Flow::Kill => {
                        self.killed = true;
                        if !stack.is_empty() {
                            // Threads that stop leave their tokens behind.
                        }
                        return Ok(());
                    }
                }
            }
            let _ = issue;
            if ctl {
                // Control flow takes 13 cycles; the code must stall them.
                if stall < 13 {
                    return Err(at("control flow with a stall under 13"));
                }
            }
            pc = next;
        }
    }
}

fn index_of(addr: i64) -> usize {
    let a = addr as usize;
    a / 32 * 3 + (a % 32 - 8) / 8
}

enum Flow {
    Next,
    Exit,
    Jump(i64),
    Ssy(i64),
    Pbk(i64),
    Pcnt(i64),
    Sync,
    Brk,
    Cont,
    Kill,
}

struct Eff {
    reads: Vec<usize>,
    writes: Vec<usize>,
    preads: Vec<usize>,
    pwrites: Vec<usize>,
    var: bool,
    store: bool,
    ctl: bool,
}

impl Sim<'_> {
    fn effects(&self, w: u64, op: u16) -> Result<Eff, String> {
        let d = bits(w, 0, 8) as usize;
        let a = bits(w, 8, 8) as usize;
        let b = bits(w, 20, 8) as usize;
        let c = bits(w, 39, 8) as usize;
        let mut e = Eff {
            reads: Vec::new(),
            writes: Vec::new(),
            preads: Vec::new(),
            pwrites: Vec::new(),
            var: false,
            store: false,
            ctl: false,
        };
        let p39 = bits(w, 39, 3) as usize;
        match family(op) {
            Op::Fadd
            | Op::Fmul
            | Op::Fmnmx
            | Op::Iadd
            | Op::Imnmx
            | Op::Lop
            | Op::Shl
            | Op::Shr
            | Op::Sel => {
                e.reads = std::vec![a, b];
                e.writes = std::vec![d];
                if matches!(family(op), Op::Fmnmx | Op::Imnmx | Op::Sel) {
                    e.preads.push(p39);
                }
            }
            Op::IaddC => {
                e.reads = std::vec![a];
                e.writes = std::vec![d];
            }
            Op::Imul => {
                e.reads = std::vec![a, b];
                e.writes = std::vec![d];
                e.var = true;
            }
            Op::Ffma => {
                e.reads = std::vec![a, b, c];
                e.writes = std::vec![d];
            }
            Op::Mov => {
                e.reads = std::vec![b];
                e.writes = std::vec![d];
            }
            Op::MovC | Op::Mov32i => {
                e.writes = std::vec![d];
            }
            Op::Lop32i => {
                e.reads = std::vec![a];
                e.writes = std::vec![d];
            }
            Op::Iadd32i | Op::IaddI | Op::ShlI => {
                e.reads = std::vec![a];
                e.writes = std::vec![d];
            }
            Op::Isetp | Op::Fsetp => {
                e.reads = std::vec![a, b];
                e.preads.push(p39);
                e.pwrites.push(bits(w, 3, 3) as usize);
            }
            Op::IsetpI | Op::IsetpC => {
                e.reads = std::vec![a];
                e.preads.push(p39);
                e.pwrites.push(bits(w, 3, 3) as usize);
            }
            Op::Psetp => {
                e.preads = std::vec![
                    bits(w, 12, 3) as usize,
                    bits(w, 29, 3) as usize,
                    bits(w, 39, 3) as usize
                ];
                e.pwrites.push(bits(w, 3, 3) as usize);
            }
            Op::Mufu | Op::Rro | Op::F2f | Op::F2i | Op::I2f | Op::I2i => {
                e.reads = std::vec![if matches!(family(op), Op::Mufu) { a } else { b }];
                e.writes = std::vec![d];
                e.var = true;
            }
            Op::S2r | Op::Ald | Op::Ipa | Op::Ldc => {
                e.writes = std::vec![d];
                e.var = true;
                if matches!(family(op), Op::Ipa) {
                    e.reads = std::vec![b];
                }
                if matches!(family(op), Op::Ldc) {
                    e.reads = std::vec![a];
                }
            }
            Op::Ast => {
                e.reads = std::vec![d];
                e.store = true;
            }
            Op::Ldg => {
                let n = words_of(bits(w, 48, 3)) as usize;
                e.reads = std::vec![a, a + 1];
                e.writes = (0..n).map(|k| d + k).collect();
                e.var = true;
            }
            Op::Stg => {
                let n = words_of(bits(w, 48, 3)) as usize;
                e.reads = (0..n).map(|k| d + k).chain([a, a + 1]).collect();
                e.store = true;
            }
            Op::Atom => {
                e.reads = std::vec![a, a + 1, b];
                e.writes = std::vec![d];
                e.var = true;
            }
            Op::Tex => {
                e.reads = std::vec![a, a + 1, a + 2, b];
                e.writes = (0..4).map(|k| d + k).collect();
                e.var = true;
            }
            Op::Nop => {}
            Op::Bra
            | Op::Ssy
            | Op::Pbk
            | Op::Pcnt
            | Op::Sync
            | Op::Brk
            | Op::Cont
            | Op::Exit
            | Op::Kil => e.ctl = true,
            Op::Unknown => return Err(format!("unknown instruction {w:016x}")),
        }
        Ok(e)
    }

    fn src_b(&self, w: u64, op: u16) -> u32 {
        // The second source of an ALU op in its register, immediate or
        // constant-buffer form.
        match op >> 12 {
            0x5 => self.regs[bits(w, 20, 8) as usize],
            0x4 => self.cb(bits(w, 34, 5) as usize, bits(w, 20, 14) * 4),
            _ => {
                let v = bits(w, 20, 19) as u32 | (bits(w, 56, 1) as u32) << 19;
                // Sign-extend 20 bits.
                ((v << 12) as i32 >> 12) as u32
            }
        }
    }

    fn exec(
        &mut self,
        w: u64,
        op: u16,
        preds: &mut [bool; 8],
        cc: &mut bool,
    ) -> Result<Flow, String> {
        let d = bits(w, 0, 8) as usize;
        let a = bits(w, 8, 8) as usize;
        let setd = |s: &mut Self, v: u32| {
            if d != 255 {
                s.regs[d] = v;
            }
        };
        let ra = self.regs[a];
        let pred = |preds: &[bool; 8], idx: u64, not: u64| preds[idx as usize] != (not == 1);
        match family(op) {
            Op::Fadd => {
                let y = f(self.src_b(w, op));
                let y = if bits(w, 45, 1) == 1 { -y } else { y };
                let mut r = f(ra) + y;
                if bits(w, 50, 1) == 1 {
                    // A NaN saturates to 0, as the hardware's .SAT does.
                    #[allow(clippy::manual_clamp)]
                    {
                        r = r.max(0.0).min(1.0);
                    }
                }
                setd(self, r.to_bits());
            }
            Op::Fmul => {
                let mut r = f(ra) * f(self.src_b(w, op));
                if bits(w, 50, 1) == 1 {
                    // A NaN saturates to 0, as the hardware's .SAT does.
                    #[allow(clippy::manual_clamp)]
                    {
                        r = r.max(0.0).min(1.0);
                    }
                }
                setd(self, r.to_bits());
            }
            Op::Fmnmx => {
                let (x, y) = (f(ra), f(self.src_b(w, op)));
                let min = pred(preds, bits(w, 39, 3), bits(w, 42, 1));
                setd(self, if min { x.min(y) } else { x.max(y) }.to_bits());
            }
            Op::Ffma => {
                let (x, y, z) = (
                    f(ra),
                    f(self.regs[bits(w, 20, 8) as usize]),
                    f(self.regs[bits(w, 39, 8) as usize]),
                );
                setd(self, ((x as f64 * y as f64 + z as f64) as f32).to_bits());
            }
            Op::Iadd => {
                let y = self.src_b(w, op);
                let y = if bits(w, 48, 1) == 1 {
                    y.wrapping_neg()
                } else {
                    y
                };
                setd(self, ra.wrapping_add(y));
            }
            Op::IaddC | Op::IaddI | Op::Iadd32i => {
                let (y, x) = match family(op) {
                    Op::Iadd32i => (bits(w, 20, 32) as u32, ra),
                    Op::IaddI => (self.src_b(w, op), ra),
                    _ => (self.src_b(w, op), ra),
                };
                let carry_in = bits(w, 43, 1) == 1 && family(op) == Op::IaddC;
                let carry_out = bits(w, 47, 1) == 1 && family(op) == Op::IaddC;
                let (s1, c1) = x.overflowing_add(y);
                let (s2, c2) = s1.overflowing_add(carry_in as u32 & *cc as u32);
                if carry_out {
                    *cc = c1 | c2;
                } else if carry_in {
                    // .X consumes the carry.
                }
                setd(self, s2);
            }
            Op::Imul => {
                let y = self.src_b(w, op);
                setd(self, ra.wrapping_mul(y));
            }
            Op::Imnmx => {
                let y = self.src_b(w, op);
                let min = pred(preds, bits(w, 39, 3), bits(w, 42, 1));
                let signed = bits(w, 48, 1) == 1;
                let r = if signed {
                    let (x, y) = (ra as i32, y as i32);
                    (if min { x.min(y) } else { x.max(y) }) as u32
                } else if min {
                    ra.min(y)
                } else {
                    ra.max(y)
                };
                setd(self, r);
            }
            Op::Lop => {
                let mut x = ra;
                let mut y = self.src_b(w, op);
                if bits(w, 39, 1) == 1 {
                    x = !x;
                }
                if bits(w, 40, 1) == 1 {
                    y = !y;
                }
                setd(
                    self,
                    match bits(w, 41, 2) {
                        0 => x & y,
                        1 => x | y,
                        2 => x ^ y,
                        _ => y,
                    },
                );
            }
            Op::Lop32i => {
                let y = bits(w, 20, 32) as u32;
                setd(
                    self,
                    match bits(w, 53, 2) {
                        0 => ra & y,
                        1 => ra | y,
                        _ => ra ^ y,
                    },
                );
            }
            Op::Shl | Op::ShlI => {
                let n = self.src_b(w, op);
                let n = if bits(w, 39, 1) == 1 || family(op) == Op::ShlI {
                    n & 31
                } else {
                    n
                };
                setd(self, if n >= 32 { 0 } else { ra << n });
            }
            Op::Shr => {
                let n = self.src_b(w, op) & 31;
                setd(
                    self,
                    if bits(w, 48, 1) == 1 {
                        ((ra as i32) >> n) as u32
                    } else {
                        ra >> n
                    },
                );
            }
            Op::Mov => setd(self, self.regs[bits(w, 20, 8) as usize]),
            Op::MovC => setd(self, self.cb(bits(w, 34, 5) as usize, bits(w, 20, 14) * 4)),
            Op::Mov32i => setd(self, bits(w, 20, 32) as u32),
            Op::Sel => {
                let y = self.src_b(w, op);
                let p = pred(preds, bits(w, 39, 3), bits(w, 42, 1));
                setd(self, if p { ra } else { y });
            }
            Op::Isetp | Op::IsetpI | Op::IsetpC => {
                let y = self.src_b(w, op);
                let signed = bits(w, 48, 1) == 1;
                let r = match bits(w, 49, 3) {
                    1 => {
                        if signed {
                            (ra as i32) < (y as i32)
                        } else {
                            ra < y
                        }
                    }
                    2 => ra == y,
                    3 => {
                        if signed {
                            (ra as i32) <= (y as i32)
                        } else {
                            ra <= y
                        }
                    }
                    4 => {
                        if signed {
                            (ra as i32) > (y as i32)
                        } else {
                            ra > y
                        }
                    }
                    5 => ra != y,
                    6 => {
                        if signed {
                            (ra as i32) >= (y as i32)
                        } else {
                            ra >= y
                        }
                    }
                    7 => true,
                    _ => false,
                };
                let acc = pred(preds, bits(w, 39, 3), bits(w, 42, 1));
                let p = bits(w, 3, 3) as usize;
                if p != 7 {
                    preds[p] = r && acc;
                }
            }
            Op::Fsetp => {
                let (x, y) = (f(ra), f(self.src_b(w, op)));
                let r = match bits(w, 48, 4) {
                    0x01 => x < y,
                    0x02 => x == y,
                    0x03 => x <= y,
                    0x04 => x > y,
                    0x05 => x != y && !x.is_nan() && !y.is_nan(),
                    0x06 => x >= y,
                    0x0D => x != y,
                    c => return Err(format!("FSETP code {c}")),
                };
                let acc = pred(preds, bits(w, 39, 3), bits(w, 42, 1));
                let p = bits(w, 3, 3) as usize;
                if p != 7 {
                    preds[p] = r && acc;
                }
            }
            Op::Psetp => {
                let s0 = pred(preds, bits(w, 12, 3), bits(w, 15, 1));
                let s1 = pred(preds, bits(w, 29, 3), bits(w, 32, 1));
                let s2 = pred(preds, bits(w, 39, 3), bits(w, 42, 1));
                let op = |k: u64, x: bool, y: bool| match k {
                    0 => x && y,
                    1 => x || y,
                    _ => x != y,
                };
                let t = op(bits(w, 24, 2), s0, s1);
                let r = op(bits(w, 45, 2), t, s2);
                let r2 = op(bits(w, 45, 2), !t, s2);
                let (d1, d2) = (bits(w, 3, 3) as usize, bits(w, 0, 3) as usize);
                if d1 != 7 {
                    preds[d1] = r;
                }
                if d2 != 7 {
                    preds[d2] = r2;
                }
            }
            Op::Mufu => {
                let x = f(ra);
                let r = match bits(w, 20, 4) {
                    0 => crate::fmath::cos(x * (2.0 * core::f32::consts::PI)),
                    1 => crate::fmath::sin(x * (2.0 * core::f32::consts::PI)),
                    2 => crate::fmath::ex2(x),
                    3 => crate::fmath::lg2(x),
                    4 => 1.0 / x,
                    5 => crate::fmath::rsq(x),
                    8 => crate::fmath::sqrt(x),
                    k => return Err(format!("MUFU {k}")),
                };
                setd(self, r.to_bits());
            }
            Op::Rro => {
                // The input is in radians: MUFU takes turns, EX2 as is.
                let x = f(self.regs[bits(w, 20, 8) as usize]);
                let r = if bits(w, 39, 1) == 1 {
                    x
                } else {
                    x * (0.5 / core::f32::consts::PI)
                };
                setd(self, r.to_bits());
            }
            Op::F2f => {
                let x = f(self.regs[bits(w, 20, 8) as usize]);
                setd(self, crate::fmath::floor(x).to_bits());
            }
            Op::F2i => {
                let x = f(self.regs[bits(w, 20, 8) as usize]);
                setd(
                    self,
                    if bits(w, 12, 1) == 1 {
                        (x as i32) as u32
                    } else {
                        x as u32
                    },
                );
            }
            Op::I2f => {
                let x = self.regs[bits(w, 20, 8) as usize];
                setd(
                    self,
                    if bits(w, 13, 1) == 1 {
                        (x as i32) as f32
                    } else {
                        x as f32
                    }
                    .to_bits(),
                );
            }
            Op::I2i => {
                let x = self.regs[bits(w, 20, 8) as usize] as i32;
                setd(
                    self,
                    if bits(w, 49, 1) == 1 {
                        x.wrapping_abs() as u32
                    } else {
                        x as u32
                    },
                );
            }
            Op::S2r => {
                let sr = bits(w, 20, 8) as u8;
                let v = *self.sregs.get(&sr).ok_or("S2R of an unset register")?;
                setd(self, v);
            }
            Op::Ald => {
                let addr = bits(w, 20, 10) as u32;
                let n = bits(w, 47, 2) as u32 + 1;
                for k in 0..n {
                    let v = *self
                        .attrs_in
                        .get(&(addr + 4 * k))
                        .ok_or("ALD of an unset attribute")?;
                    self.regs[d + k as usize] = v;
                }
            }
            Op::Ast => {
                let addr = bits(w, 20, 10) as u32;
                let n = bits(w, 47, 2) as u32 + 1;
                for k in 0..n {
                    self.attrs_out
                        .insert(addr + 4 * k, self.regs[d + k as usize]);
                }
            }
            Op::Ipa => {
                let addr = bits(w, 28, 10) as u32;
                let v = *self
                    .attrs_in
                    .get(&addr)
                    .ok_or("IPA of an unset attribute")?;
                let mode = bits(w, 54, 2);
                let r = if mode == 1 {
                    // Perspective: times the register in 20..28 (w).
                    (f(v) * f(self.regs[bits(w, 20, 8) as usize])).to_bits()
                } else {
                    v
                };
                setd(self, r);
            }
            Op::Ldc => {
                let off = bits(w, 20, 16) + self.regs[a] as u64;
                let idx = bits(w, 36, 5) as usize;
                setd(self, self.cb(idx, off));
            }
            Op::Ldg => {
                let n = words_of(bits(w, 48, 3)) as usize;
                let addr = self.regs[a] as u64 | (self.regs[a + 1] as u64) << 32;
                if !addr.is_multiple_of(4 * n as u64) {
                    return Err(format!("misaligned {n}-word load at {addr:#x}"));
                }
                let vals: Vec<u32> = self
                    .mem_ptr(addr, n)
                    .ok_or(format!("load outside any buffer at {addr:#x}"))?
                    .to_vec();
                for (k, v) in vals.into_iter().enumerate() {
                    self.regs[d + k] = v;
                }
            }
            Op::Stg => {
                let n = words_of(bits(w, 48, 3)) as usize;
                let addr = self.regs[a] as u64 | (self.regs[a + 1] as u64) << 32;
                if !addr.is_multiple_of(4 * n as u64) {
                    return Err(format!("misaligned {n}-word store at {addr:#x}"));
                }
                let vals: Vec<u32> = (0..n).map(|k| self.regs[d + k]).collect();
                self.mem_ptr(addr, n)
                    .ok_or(format!("store outside any buffer at {addr:#x}"))?
                    .copy_from_slice(&vals);
            }
            Op::Atom => {
                let addr = self.regs[a] as u64 | (self.regs[a + 1] as u64) << 32;
                let add = self.regs[bits(w, 20, 8) as usize];
                let m = self
                    .mem_ptr(addr, 1)
                    .ok_or(format!("atomic outside any buffer at {addr:#x}"))?;
                let old = m[0];
                m[0] = old.wrapping_add(add);
                setd(self, old);
            }
            Op::Tex => {
                let handle = self.regs[bits(w, 20, 8) as usize];
                let lod_mode = bits(w, 37, 2);
                let (u, v) = (f(self.regs[a]), f(self.regs[a + 1]));
                let lod = (lod_mode == 3).then(|| f(self.regs[a + 2]));
                let t = self.sampler.sample(handle as u8, u, v, lod);
                for (k, x) in t.into_iter().enumerate() {
                    self.regs[d + k] = x.to_bits();
                }
            }
            Op::Nop => {}
            Op::Bra => return Ok(Flow::Jump(self.target(w))),
            Op::Ssy => return Ok(Flow::Ssy(self.target(w))),
            Op::Pbk => return Ok(Flow::Pbk(self.target(w))),
            Op::Pcnt => return Ok(Flow::Pcnt(self.target(w))),
            Op::Sync => return Ok(Flow::Sync),
            Op::Brk => return Ok(Flow::Brk),
            Op::Cont => return Ok(Flow::Cont),
            Op::Exit => return Ok(Flow::Exit),
            Op::Kil => return Ok(Flow::Kill),
            Op::Unknown => return Err(format!("unknown instruction {w:016x}")),
        }
        Ok(Flow::Next)
    }

    /// Absolute byte address a control op targets (see `Asm::finish`).
    fn target(&self, w: u64) -> i64 {
        let rel = bits(w, 20, 24) as i64;
        let rel = (rel << 40) >> 40;
        rel + self.cur_addr.get() + 8
    }
}

fn words_of(mem_type: u64) -> u8 {
    match mem_type {
        4 => 1,
        5 => 2,
        6 => 4,
        _ => 1,
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Op {
    Fadd,
    Fmul,
    Fmnmx,
    Ffma,
    Iadd,
    IaddC,
    IaddI,
    Iadd32i,
    Imul,
    Imnmx,
    Lop,
    Lop32i,
    Shl,
    ShlI,
    Shr,
    Mov,
    MovC,
    Mov32i,
    Sel,
    Isetp,
    IsetpI,
    IsetpC,
    Fsetp,
    Psetp,
    Mufu,
    Rro,
    F2f,
    F2i,
    I2f,
    I2i,
    S2r,
    Ald,
    Ast,
    Ipa,
    Ldc,
    Ldg,
    Stg,
    Atom,
    Tex,
    Nop,
    Bra,
    Ssy,
    Pbk,
    Pcnt,
    Sync,
    Brk,
    Cont,
    Exit,
    Kil,
    Unknown,
}

fn family(op: u16) -> Op {
    let m = |mask: u16, v: u16| op & mask == v;
    if m(0xFFF8, 0x5C58) {
        Op::Fadd
    } else if m(0xFFF8, 0x5C68) {
        Op::Fmul
    } else if m(0xFFF8, 0x5C60) {
        Op::Fmnmx
    } else if m(0xFFE0, 0x5980) {
        Op::Ffma
    } else if m(0xFFFC, 0x5C10) {
        Op::Iadd
    } else if m(0xFFFC, 0x4C10) {
        Op::IaddC
    } else if m(0xFEFC, 0x3810) {
        Op::IaddI
    } else if op & 0xFF00 == 0x1C00 {
        Op::Iadd32i
    } else if m(0xFFFF, 0x5C38) {
        Op::Imul
    } else if m(0xFFFE, 0x5C20) {
        Op::Imnmx
    } else if m(0xFFF8, 0x5C40) {
        Op::Lop
    } else if op & 0xFC00 == 0x0400 {
        Op::Lop32i
    } else if m(0xFFFF, 0x5C48) {
        Op::Shl
    } else if m(0xFEFF, 0x3848) {
        Op::ShlI
    } else if m(0xFFFE, 0x5C28) {
        Op::Shr
    } else if m(0xFFFF, 0x5C98) {
        Op::Mov
    } else if m(0xFFFF, 0x4C98) {
        Op::MovC
    } else if op & 0xFF00 == 0x0100 {
        Op::Mov32i
    } else if m(0xFFFF, 0x5CA0) {
        Op::Sel
    } else if m(0xFFF0, 0x5B60) {
        Op::Isetp
    } else if m(0xFEF0, 0x3660) {
        Op::IsetpI
    } else if m(0xFFF0, 0x4B60) {
        Op::IsetpC
    } else if m(0xFFF0, 0x5BB0) {
        Op::Fsetp
    } else if m(0xFFFF, 0x5090) {
        Op::Psetp
    } else if m(0xFFFF, 0x5080) {
        Op::Mufu
    } else if m(0xFFFF, 0x5C90) {
        Op::Rro
    } else if m(0xFFFF, 0x5CA8) {
        Op::F2f
    } else if m(0xFFFF, 0x5CB0) {
        Op::F2i
    } else if m(0xFFFF, 0x5CB8) {
        Op::I2f
    } else if m(0xFFF8, 0x5CE0) {
        Op::I2i
    } else if m(0xFFFF, 0xF0C8) {
        Op::S2r
    } else if m(0xFFFE, 0xEFD8) {
        Op::Ald
    } else if m(0xFFFE, 0xEFF0) {
        Op::Ast
    } else if m(0xFF3C, 0xE000) {
        Op::Ipa
    } else if m(0xFFF8, 0xEF90) {
        Op::Ldc
    } else if m(0xFFF8, 0xEED0) {
        Op::Ldg
    } else if m(0xFFF8, 0xEED8) {
        Op::Stg
    } else if m(0xFF00, 0xED00) {
        Op::Atom
    } else if m(0xFFFF, 0xDEB8) {
        Op::Tex
    } else if m(0xFFFF, 0x50B0) {
        Op::Nop
    } else if m(0xFFFF, 0xE240) {
        Op::Bra
    } else if m(0xFFFF, 0xE290) {
        Op::Ssy
    } else if m(0xFFFF, 0xE2A0) {
        Op::Pbk
    } else if m(0xFFFF, 0xE2B0) {
        Op::Pcnt
    } else if m(0xFFFF, 0xF0F8) {
        Op::Sync
    } else if m(0xFFFF, 0xE340) {
        Op::Brk
    } else if m(0xFFFF, 0xE350) {
        Op::Cont
    } else if m(0xFFFF, 0xE300) {
        Op::Exit
    } else if m(0xFFFF, 0xE330) {
        Op::Kil
    } else {
        Op::Unknown
    }
}
