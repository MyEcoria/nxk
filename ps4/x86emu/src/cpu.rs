//! Interpreter for the instructions `decode` understands.

use crate::decode::{decode, AluOp, DecodeError, Inst, MemRef, Op, Operand};

pub const CF: u64 = 1;
pub const PF: u64 = 1 << 2;
pub const AF: u64 = 1 << 4;
pub const ZF: u64 = 1 << 6;
pub const SF: u64 = 1 << 7;
pub const OF: u64 = 1 << 11;
/// The arithmetic flags the interpreter keeps.
pub const FLAG_MASK: u64 = CF | PF | AF | ZF | SF | OF;

/// Guest memory as the interpreter sees it.
pub trait Mem {
    /// Read `size` (1..=8) bytes, little endian. `None` when unmapped.
    fn read(&mut self, addr: u64, size: u8) -> Option<u64>;
    /// Write the low `size` bytes of `value`. `false` when unmapped.
    fn write(&mut self, addr: u64, size: u8, value: u64) -> bool;
}

/// Why execution stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stop {
    /// A `ret` popped the sentinel return address.
    Returned,
    /// Access to unmapped memory.
    Fault(u64),
    Decode(u64, DecodeError),
    /// The instruction budget ran out.
    Budget,
}

const ICACHE_LINES: usize = 256;

/// Direct-mapped cache of decoded instructions, keyed by rip.
///
/// Writes made by the guest itself to a page holding cached code flush it; writes from
/// elsewhere (DMA, other threads) must call `Cpu::flush_icache`.
#[derive(Clone)]
struct ICache {
    lines: [Option<(u64, Inst)>; ICACHE_LINES],
    /// One bit per hashed code page: a write to a page whose bit is set flushes the cache.
    code_pages: u64,
}

impl Default for ICache {
    fn default() -> Self {
        ICache {
            lines: [None; ICACHE_LINES],
            code_pages: 0,
        }
    }
}

fn page_bit(addr: u64) -> u64 {
    1 << ((addr >> 12).wrapping_mul(0x9e37_79b9_7f4a_7c15) >> 58)
}

/// Architectural state: 16 GPRs (rax, rcx, rdx, rbx, rsp, rbp, rsi, rdi, r8..r15), rip, flags.
#[derive(Clone, Default)]
pub struct Cpu {
    pub gpr: [u64; 16],
    pub rip: u64,
    pub flags: u64,
    icache: ICache,
}

fn mask(size: u8) -> u64 {
    if size == 8 {
        u64::MAX
    } else {
        (1u64 << (8 * size as u32)) - 1
    }
}

fn parity(v: u64) -> bool {
    (v as u8).count_ones().is_multiple_of(2)
}

/// Result and arithmetic flags of an ALU operation on `size`-byte operands.
fn alu(op: AluOp, a: u64, b: u64, cf_in: bool, size: u8) -> (u64, u64) {
    let (m, bits) = (mask(size), 8 * size as u32);
    let (a, b) = (a & m, b & m);
    let sign = 1u64 << (bits - 1);
    let (res, cf, of, af);
    match op {
        AluOp::Add | AluOp::Adc => {
            let c = (op == AluOp::Adc && cf_in) as u64;
            let wide = a as u128 + b as u128 + c as u128;
            res = (wide as u64) & m;
            cf = wide >> bits != 0;
            of = (!(a ^ b) & (a ^ res) & sign) != 0;
            af = ((a ^ b ^ res) & 0x10) != 0;
        }
        AluOp::Sub | AluOp::Sbb | AluOp::Cmp => {
            let c = (op == AluOp::Sbb && cf_in) as u64;
            res = a.wrapping_sub(b).wrapping_sub(c) & m;
            cf = (a as u128) < (b as u128 + c as u128);
            of = ((a ^ b) & (a ^ res) & sign) != 0;
            af = ((a ^ b ^ res) & 0x10) != 0;
        }
        AluOp::And | AluOp::Or | AluOp::Xor => {
            res = match op {
                AluOp::And => a & b,
                AluOp::Or => a | b,
                _ => a ^ b,
            };
            (cf, of, af) = (false, false, false);
        }
    }
    let mut f = 0;
    for (on, bit) in [
        (cf, CF),
        (parity(res), PF),
        (af, AF),
        (res == 0, ZF),
        (res & sign != 0, SF),
        (of, OF),
    ] {
        if on {
            f |= bit;
        }
    }
    (res, f)
}

impl Cpu {
    /// A CPU with the given registers and an empty instruction cache.
    pub fn new(gpr: [u64; 16], rip: u64, flags: u64) -> Cpu {
        Cpu {
            gpr,
            rip,
            flags,
            icache: ICache::default(),
        }
    }

    /// Drop every cached decoded instruction (call after code changed behind the CPU's back).
    pub fn flush_icache(&mut self) {
        self.icache = ICache::default();
    }

    fn note_write(&mut self, addr: u64, size: u8) {
        let last = addr.wrapping_add(size as u64 - 1);
        if self.icache.code_pages & (page_bit(addr) | page_bit(last)) != 0 {
            self.flush_icache();
        }
    }

    fn fetch(&mut self, mem: &mut dyn Mem) -> Result<Inst, Stop> {
        let slot = (self.rip as usize ^ (self.rip >> 12) as usize) % ICACHE_LINES;
        if let Some((rip, inst)) = &self.icache.lines[slot] {
            if *rip == self.rip {
                return Ok(*inst);
            }
        }
        let mut buf = [0u8; 15];
        let mut n = 0;
        while n < 15 {
            match mem.read(self.rip.wrapping_add(n as u64), 1) {
                Some(b) => buf[n] = b as u8,
                None => break,
            }
            n += 1;
        }
        let inst = decode(&buf[..n]).map_err(|e| match e {
            DecodeError::Truncated if n == 0 => Stop::Fault(self.rip),
            e => Stop::Decode(self.rip, e),
        })?;
        let end = self.rip.wrapping_add(inst.len as u64 - 1);
        self.icache.code_pages |= page_bit(self.rip) | page_bit(end);
        self.icache.lines[slot] = Some((self.rip, inst));
        Ok(inst)
    }

    fn ea(&self, m: &MemRef, next: u64) -> u64 {
        let mut a = m.disp as u64;
        if m.rip {
            a = a.wrapping_add(next);
        }
        if let Some(b) = m.base {
            a = a.wrapping_add(self.gpr[b as usize]);
        }
        if let Some((i, s)) = m.index {
            a = a.wrapping_add(self.gpr[i as usize].wrapping_mul(s as u64));
        }
        a
    }

    fn get(&self, o: Operand, size: u8, mem: &mut dyn Mem, next: u64) -> Result<u64, Stop> {
        match o {
            Operand::Reg(r) => Ok(self.gpr[r as usize] & mask(size)),
            Operand::Imm(v) => Ok(v as u64 & mask(size)),
            Operand::Mem(m) => {
                let a = self.ea(&m, next);
                mem.read(a, size).ok_or(Stop::Fault(a))
            }
        }
    }

    fn put(
        &mut self,
        o: Operand,
        size: u8,
        v: u64,
        mem: &mut dyn Mem,
        next: u64,
    ) -> Result<(), Stop> {
        match o {
            // A 32-bit register write clears the upper half.
            Operand::Reg(r) => self.gpr[r as usize] = v & mask(size),
            Operand::Mem(m) => {
                let a = self.ea(&m, next);
                if !mem.write(a, size, v) {
                    return Err(Stop::Fault(a));
                }
                self.note_write(a, size);
            }
            Operand::Imm(_) => unreachable!("decoder never yields an immediate destination"),
        }
        Ok(())
    }

    fn cond(&self, cc: u8) -> bool {
        let f = |b| self.flags & b != 0;
        let r = match cc >> 1 {
            0 => f(OF),
            1 => f(CF),
            2 => f(ZF),
            3 => f(CF) || f(ZF),
            4 => f(SF),
            5 => f(PF),
            6 => f(SF) != f(OF),
            _ => f(ZF) || f(SF) != f(OF),
        };
        r != (cc & 1 != 0)
    }

    fn push(&mut self, v: u64, mem: &mut dyn Mem) -> Result<(), Stop> {
        let sp = self.gpr[4].wrapping_sub(8);
        if !mem.write(sp, 8, v) {
            return Err(Stop::Fault(sp));
        }
        self.gpr[4] = sp;
        self.note_write(sp, 8);
        Ok(())
    }

    fn pop(&mut self, mem: &mut dyn Mem) -> Result<u64, Stop> {
        let sp = self.gpr[4];
        let v = mem.read(sp, 8).ok_or(Stop::Fault(sp))?;
        self.gpr[4] = sp.wrapping_add(8);
        Ok(v)
    }

    /// Execute one decoded instruction located at `rip`.
    ///
    /// # Returns
    /// `Ok(true)` when a `ret` popped `sentinel`, `Ok(false)` otherwise.
    fn exec(&mut self, i: Inst, mem: &mut dyn Mem, sentinel: u64) -> Result<bool, Stop> {
        let next = self.rip.wrapping_add(i.len as u64);
        self.rip = next;
        let sz = i.size;
        match i.op {
            Op::Nop => {}
            Op::Alu(op) => {
                let (a, b) = (
                    self.get(i.dst, sz, mem, next)?,
                    self.get(i.src, sz, mem, next)?,
                );
                let (res, f) = alu(op, a, b, self.flags & CF != 0, sz);
                self.flags = (self.flags & !FLAG_MASK) | f;
                if op != AluOp::Cmp {
                    self.put(i.dst, sz, res, mem, next)?;
                }
            }
            Op::Test => {
                let (a, b) = (
                    self.get(i.dst, sz, mem, next)?,
                    self.get(i.src, sz, mem, next)?,
                );
                self.flags = (self.flags & !FLAG_MASK) | alu(AluOp::And, a, b, false, sz).1;
            }
            Op::Mov => {
                let v = self.get(i.src, sz, mem, next)?;
                self.put(i.dst, sz, v, mem, next)?;
            }
            Op::Lea => {
                let Operand::Mem(m) = i.src else {
                    unreachable!("decoder: lea source is memory")
                };
                let a = self.ea(&m, next);
                self.put(i.dst, sz, a & mask(sz), mem, next)?;
            }
            Op::Push => {
                let v = self.get(i.dst, 8, mem, next)?;
                self.push(v, mem)?;
            }
            Op::Pop => {
                let v = self.pop(mem)?;
                self.put(i.dst, 8, v, mem, next)?;
            }
            Op::Ret => {
                let target = self.pop(mem)?;
                if target == sentinel {
                    return Ok(true);
                }
                self.rip = target;
            }
            Op::Jmp => self.rip = next.wrapping_add(self.get(i.dst, 8, mem, next)?),
            Op::Jcc(cc) => {
                if self.cond(cc) {
                    self.rip = next.wrapping_add(self.get(i.dst, 8, mem, next)?);
                }
            }
        }
        Ok(false)
    }

    /// Run from `self.rip` until a `ret` pops `sentinel`.
    ///
    /// # Args
    /// * `mem` - guest memory; code is read from it too
    /// * `sentinel` - return address that ends the run
    /// * `max_steps` - instruction budget (guards against infinite loops)
    ///
    /// # Returns
    /// `Ok(())` once the sentinel is returned to, otherwise why it stopped.
    pub fn run(&mut self, mem: &mut dyn Mem, sentinel: u64, max_steps: u64) -> Result<(), Stop> {
        for _ in 0..max_steps {
            let inst = self.fetch(mem)?;
            if self.exec(inst, mem, sentinel)? {
                return Ok(());
            }
        }
        Err(Stop::Budget)
    }
}
