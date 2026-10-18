//! Issuing SASS in order with its hazards covered. Each instruction
//! names the registers it reads and writes; this works out the stall
//! cycles before it (fixed-latency results: 6 cycles for a GPR, 13 for a
//! predicate) and the scoreboards it waits on (results of loads,
//! texture fetches and the multi-function unit), as NAK's
//! `calc_instr_deps` does but with simpler bookkeeping: at a label or
//! after a branch everything in flight is waited for.

use crate::sass::*;

const NONE: u8 = 0xFF;
/// Cycles until a fixed-latency GPR result / predicate is readable.
const GPR_LATENCY: u32 = 6;
const PRED_LATENCY: u32 = 13;
/// Cycles in which a variable-latency instruction may still read its
/// sources.
const READ_WINDOW: u32 = 4;
/// Stall after a control-flow instruction.
const CTL_STALL: u8 = 15;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// Result after a fixed number of cycles.
    Fixed,
    /// Result (if any) signalled on a scoreboard: loads, MUFU, TEX, ...
    Var,
    /// Reads its sources late (stores, attribute stores).
    Store,
    /// Branch and friends.
    Ctl,
}

/// The registers an instruction touches (`RZ` entries are ignored).
pub struct Eff<'a> {
    pub kind: Kind,
    pub r: &'a [u8],
    pub w: &'a [u8],
    pub pr: &'a [u8],
    pub pw: &'a [u8],
}

impl<'a> Eff<'a> {
    pub fn fixed(w: &'a [u8], r: &'a [u8]) -> Self {
        Self {
            kind: Kind::Fixed,
            r,
            w,
            pr: &[],
            pw: &[],
        }
    }

    pub fn var(w: &'a [u8], r: &'a [u8]) -> Self {
        Self {
            kind: Kind::Var,
            r,
            w,
            pr: &[],
            pw: &[],
        }
    }

    pub fn store(r: &'a [u8]) -> Self {
        Self {
            kind: Kind::Store,
            r,
            w: &[],
            pr: &[],
            pw: &[],
        }
    }

    pub fn ctl(pr: &'a [u8]) -> Self {
        Self {
            kind: Kind::Ctl,
            r: &[],
            w: &[],
            pr,
            pw: &[],
        }
    }

    /// Also reads predicates `pr` / writes predicates `pw`.
    pub fn preds(mut self, pr: &'a [u8], pw: &'a [u8]) -> Self {
        self.pr = pr;
        self.pw = pw;
        self
    }
}

pub struct Emitter {
    pub a: Asm,
    /// Cycle at which the next instruction issues.
    t: u32,
    ready: [u32; 256],
    war: [u32; 256],
    pready: [u32; 8],
    pend_w: [u8; 256],
    pend_r: [u8; 256],
    next_bar: u8,
    wait_all: bool,
}

impl Default for Emitter {
    fn default() -> Self {
        Self::new()
    }
}

impl Emitter {
    pub fn new() -> Self {
        Self {
            a: Asm::new(),
            t: 0,
            ready: [0; 256],
            war: [0; 256],
            pready: [0; 8],
            pend_w: [NONE; 256],
            pend_r: [NONE; 256],
            next_bar: 0,
            // Registers may hold anything from before: nothing in flight.
            wait_all: false,
        }
    }

    /// Lengthen the stall of the last instruction by `n` cycles.
    fn bump(&mut self, mut n: u32) {
        while n > 0 {
            let room = self
                .a
                .last_sched()
                .map(|s| (15 - s.stalls()) as u32)
                .unwrap_or(0);
            if room > 0 {
                let add = room.min(n);
                let s = self.a.last_sched().unwrap();
                *s = s.with_stall(s.stalls() + add as u8);
                self.t += add;
                n -= add;
            } else {
                let k = n.min(15);
                self.a.emit(nop(), Sched::stall(k as u8));
                self.t += k;
                n -= k;
            }
        }
    }

    pub fn emit(&mut self, insn: u64, e: &Eff) {
        let mut wait = 0u8;
        let mut earliest = self.t;
        if self.wait_all {
            wait = 0x3F;
            self.wait_all = false;
        }
        let guard = (insn >> 16 & 7) as u8;
        let guard = [guard];
        let pr = e.pr.iter().chain(guard.iter().filter(|&&g| g != PT));
        for &p in pr {
            earliest = earliest.max(self.pready[p as usize]);
        }
        for &r in e.r.iter().filter(|&&r| r != RZ) {
            let b = self.pend_w[r as usize];
            if b != NONE {
                wait |= 1 << b;
            } else {
                earliest = earliest.max(self.ready[r as usize]);
            }
        }
        for &r in e.w.iter().filter(|&&r| r != RZ) {
            for b in [self.pend_w[r as usize], self.pend_r[r as usize]] {
                if b != NONE {
                    wait |= 1 << b;
                }
            }
            earliest = earliest.max(self.war[r as usize]);
        }
        if wait != 0 {
            for i in 0..256 {
                for p in [&mut self.pend_w[i], &mut self.pend_r[i]] {
                    if *p != NONE && wait >> *p & 1 != 0 {
                        *p = NONE;
                    }
                }
            }
        }
        if earliest > self.t {
            self.bump(earliest - self.t);
        }
        let issue = self.t;
        let mut s = Sched::stall(1).wait(wait);
        match e.kind {
            Kind::Fixed => {}
            Kind::Ctl => s = s.with_stall(CTL_STALL),
            Kind::Var | Kind::Store => {
                s = s.with_stall(2);
                let has_dst = e.kind == Kind::Var && e.w.iter().any(|&r| r != RZ);
                let bar = self.next_bar;
                self.next_bar = (self.next_bar + 1) % 6;
                if has_dst {
                    s = s.wr(bar);
                    for &r in e.w.iter().filter(|&&r| r != RZ) {
                        self.pend_w[r as usize] = bar;
                    }
                    for &r in e.r.iter().filter(|&&r| r != RZ) {
                        self.war[r as usize] = self.war[r as usize].max(issue + READ_WINDOW);
                    }
                } else {
                    s = s.rd(bar);
                    for &r in e.r.iter().filter(|&&r| r != RZ) {
                        self.pend_r[r as usize] = bar;
                    }
                }
            }
        }
        self.a.emit(insn, s);
        if e.kind == Kind::Fixed {
            for &r in e.w.iter().filter(|&&r| r != RZ) {
                self.ready[r as usize] = issue + GPR_LATENCY;
            }
        }
        for &p in e.pw.iter().filter(|&&p| p != PT) {
            self.pready[p as usize] = issue + PRED_LATENCY;
        }
        self.t = issue + s.stalls() as u32;
        if e.kind == Kind::Ctl {
            self.ready = [0; 256];
            self.war = [0; 256];
            self.pready = [0; 8];
            self.wait_all = true;
        }
    }

    /// Wait for everything in flight before the next instruction.
    pub fn drain(&mut self) {
        self.wait_all = true;
    }

    pub fn label(&mut self) -> Label {
        self.a.new_label()
    }

    /// Bind a label here; code after it can be reached from anywhere.
    pub fn place(&mut self, l: Label) {
        self.a.place(l);
        self.wait_all = true;
    }

    /// A branch-like instruction with a target.
    pub fn ctl(&mut self, opcode: u64, l: Label, p: u8, not: bool) {
        let preds = [p];
        let pr: &[u8] = if p == PT { &[] } else { &preds };
        // Same bookkeeping as `emit`, with the target patched at the end.
        let pos = self.a.len();
        self.emit(when(op_word(opcode), p, not), &Eff::ctl(pr));
        self.a.mark_target(pos, l);
    }

    pub fn finish(self) -> alloc::vec::Vec<u64> {
        self.a.finish()
    }
}
