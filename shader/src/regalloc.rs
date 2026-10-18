//! Registers for a program: the live range of every virtual register
//! (stretched over the loops it crosses), then linear scan over the
//! GPRs, and over the five predicates for the Bool registers (the rest
//! of those live in a GPR as 0 / 1).

use alloc::vec::Vec;

use crate::ir::*;

/// Where a virtual register lives.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Loc {
    Gpr(u8),
    Pred(u8),
}

/// Predicates handed out to Bool registers; P5 and P6 are temporaries
/// of the code generator, P7 is PT.
pub const POOL_PREDS: u8 = 5;

pub struct Alloc {
    pub locs: Vec<Loc>,
    /// The counter register of each loop, in program order.
    pub loop_counters: Vec<u8>,
    /// Highest GPR used + 1.
    pub gprs: u16,
}

#[derive(Clone, Copy)]
struct Range {
    first: u32,
    last: u32,
    used: bool,
}

struct Walk<'a> {
    p: &'a Program,
    pc: u32,
    ranges: Vec<Range>,
    loops: Vec<(u32, u32)>,
}

impl Walk<'_> {
    fn touch(&mut self, r: Reg) {
        let pc = self.pc;
        let x = &mut self.ranges[r.0 as usize];
        if !x.used {
            *x = Range {
                first: pc,
                last: pc,
                used: true,
            };
        } else {
            x.last = pc;
        }
    }

    fn body(&mut self, b: &[Inst]) {
        for i in b {
            self.pc += 1;
            visit_regs(i, &mut |r| self.touch(r));
            match i {
                Inst::If { cond, then_, else_ } => {
                    self.body(then_);
                    self.body(else_);
                    // The condition can be a guard for the whole statement,
                    // when both sides are short: a branch needs it at the
                    // jump only, and a predicate held across a long body
                    // (a traversal) is one the body cannot use.
                    if short_alu(self.p, then_) && short_alu(self.p, else_) {
                        let end = self.pc;
                        let x = &mut self.ranges[cond.0 as usize];
                        x.last = x.last.max(end);
                    }
                }
                Inst::Loop { body, .. } => {
                    let start = self.pc;
                    let slot = self.loops.len();
                    self.loops.push((start, start));
                    self.body(body);
                    self.loops[slot].1 = self.pc + 1;
                    self.pc += 1;
                }
                _ => {}
            }
        }
    }
}

/// A body that `lower` may run under a guard predicate instead of
/// branching: a few instructions of plain arithmetic on numbers.
pub fn short_alu(p: &Program, b: &[Inst]) -> bool {
    if b.len() > 6 {
        return false;
    }
    b.iter().all(|i| {
        let ok = matches!(
            i,
            Inst::Const { .. }
                | Inst::Mov { .. }
                | Inst::Un { .. }
                | Inst::Bin { .. }
                | Inst::Fma { .. }
                | Inst::Sel { .. }
        );
        let mut nums = true;
        visit_regs(i, &mut |r| {
            nums &= p.reg_types[r.0 as usize] != Ty::Bool;
        });
        ok && nums
    })
}

/// Every register an instruction names (not those of nested bodies).
pub fn visit_regs(i: &Inst, f: &mut impl FnMut(Reg)) {
    match i {
        Inst::Const { dst, .. }
        | Inst::Builtin { dst, .. }
        | Inst::Uniform { dst, .. }
        | Inst::Input { dst, .. } => f(*dst),
        Inst::Mov { dst, src } => {
            f(*src);
            f(*dst);
        }
        Inst::Un { dst, a, .. } => {
            f(*a);
            f(*dst);
        }
        Inst::Bin { dst, a, b, .. } | Inst::Cmp { dst, a, b, .. } => {
            f(*a);
            f(*b);
            f(*dst);
        }
        Inst::Fma { dst, a, b, c } => {
            f(*a);
            f(*b);
            f(*c);
            f(*dst);
        }
        Inst::Sel { dst, p, a, b } => {
            f(*p);
            f(*a);
            f(*b);
            f(*dst);
        }
        Inst::Output { src, .. } => f(*src),
        Inst::UniformIdx { dst, index, .. } => {
            f(*index);
            f(*dst);
        }
        Inst::BufLoad { index, dsts, .. } => {
            f(*index);
            dsts.iter().for_each(|d| f(*d));
        }
        Inst::BufStore { index, srcs, .. } => {
            f(*index);
            srcs.iter().for_each(|d| f(*d));
        }
        Inst::BufAtomicAdd {
            dst, index, src, ..
        } => {
            f(*index);
            f(*src);
            f(*dst);
        }
        Inst::LocalLoad { dst, index } => {
            f(*index);
            f(*dst);
        }
        Inst::LocalStore { index, src } => {
            f(*index);
            f(*src);
        }
        Inst::Tex {
            coords, lod, dsts, ..
        } => {
            coords.iter().for_each(|r| f(*r));
            lod.iter().for_each(|r| f(*r));
            dsts.iter().for_each(|r| f(*r));
        }
        Inst::If { cond, .. } => f(*cond),
        Inst::Break { cond } | Inst::Continue { cond } | Inst::Discard { cond } => {
            cond.iter().for_each(|r| f(*r))
        }
        Inst::Loop { .. } => {}
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TooMany;

/// `base`: the first GPR the program's registers may take.
pub fn allocate(p: &Program, base: u8) -> Result<Alloc, TooMany> {
    let n = p.reg_types.len();
    let mut w = Walk {
        p,
        pc: 0,
        ranges: alloc::vec![
            Range {
                first: 0,
                last: 0,
                used: false
            };
            n
        ],
        loops: Vec::new(),
    };
    w.body(&p.body);
    // A register alive when a loop starts and touched inside it is alive
    // for the whole loop: the next turn reads it again.
    for &(start, end) in &w.loops {
        for r in w.ranges.iter_mut().filter(|r| r.used) {
            if r.first < start && r.last >= start && r.last < end {
                r.last = end;
            }
        }
    }
    struct Item {
        first: u32,
        last: u32,
        v: usize,
        pred_ok: bool,
    }
    let mut items: Vec<Item> = Vec::new();
    for (v, r) in w.ranges.iter().enumerate() {
        if r.used {
            items.push(Item {
                first: r.first,
                last: r.last,
                v,
                pred_ok: p.reg_types[v] == Ty::Bool,
            });
        }
    }
    for (k, &(s, e)) in w.loops.iter().enumerate() {
        items.push(Item {
            first: s,
            last: e,
            v: n + k,
            pred_ok: false,
        });
    }
    items.sort_by_key(|i| (i.first, i.v));

    // The results of a vector load take consecutive registers, aligned to
    // the width, so that the load can write them in place.
    let mut group_of: Vec<Option<usize>> = alloc::vec![None; n];
    let mut groups: Vec<Vec<usize>> = Vec::new();
    walk(&p.body, &mut |i| {
        if let Inst::BufLoad { dsts, .. } = i {
            let mut v: Vec<usize> = dsts.iter().map(|d| d.0 as usize).collect();
            let fresh = v
                .iter()
                .all(|&d| group_of[d].is_none() && p.reg_types[d] != Ty::Bool);
            v.dedup();
            if dsts.len() > 1 && fresh && v.len() == dsts.len() {
                for &d in &v {
                    group_of[d] = Some(groups.len());
                }
                groups.push(dsts.iter().map(|d| d.0 as usize).collect());
            }
        }
    });
    let mut placed = alloc::vec![false; groups.len()];

    let mut locs = alloc::vec![Loc::Gpr(255); n];
    let mut counters = alloc::vec![255u8; w.loops.len()];
    let mut gpr_busy = [false; 256];
    let mut pred_busy = [false; POOL_PREDS as usize];
    let mut active: Vec<(u32, Loc)> = Vec::new();
    let mut top = base as u16;
    for it in &items {
        active.retain(|&(last, loc)| {
            if last < it.first {
                match loc {
                    Loc::Gpr(r) => gpr_busy[r as usize] = false,
                    Loc::Pred(q) => pred_busy[q as usize] = false,
                }
                false
            } else {
                true
            }
        });
        if it.v < n {
            if let Some(g) = group_of[it.v] {
                if placed[g] {
                    continue;
                }
                placed[g] = true;
                let width = groups[g].len();
                let r = (base as usize..=255 - width)
                    .find(|&r| r % width == 0 && (r..r + width).all(|x| !gpr_busy[x]))
                    .ok_or(TooMany)?;
                for (k, &v) in groups[g].iter().enumerate() {
                    gpr_busy[r + k] = true;
                    top = top.max((r + k) as u16 + 1);
                    locs[v] = Loc::Gpr((r + k) as u8);
                    active.push((w.ranges[v].last, Loc::Gpr((r + k) as u8)));
                }
                continue;
            }
        }
        let mut loc = None;
        if it.pred_ok {
            if let Some(q) = pred_busy.iter().position(|b| !b) {
                pred_busy[q] = true;
                loc = Some(Loc::Pred(q as u8));
            }
        }
        let loc = match loc {
            Some(l) => l,
            None => {
                let r = (base as usize..255)
                    .find(|&r| !gpr_busy[r])
                    .ok_or(TooMany)?;
                gpr_busy[r] = true;
                top = top.max(r as u16 + 1);
                Loc::Gpr(r as u8)
            }
        };
        active.push((it.last, loc));
        if it.v < n {
            locs[it.v] = loc;
        } else if let Loc::Gpr(r) = loc {
            counters[it.v - n] = r;
        }
    }
    Ok(Alloc {
        locs,
        loop_counters: counters,
        gprs: top,
    })
}
