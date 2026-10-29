//! Interpreter for the instructions `decode` understands.

use core::ops::Range;

mod float;
mod vector;

pub use float::{MXCSR_MASK, MXCSR_RESET};

use crate::decode::{
    decode, AluOp, BitOp, BmiOp, BtOp, DecodeError, Inst, MemRef, Op, Operand, Rep, Seg, ShiftOp,
    UnOp,
};

pub const CF: u64 = 1;
pub const PF: u64 = 1 << 2;
pub const AF: u64 = 1 << 4;
pub const ZF: u64 = 1 << 6;
pub const SF: u64 = 1 << 7;
pub const DF: u64 = 1 << 10;
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
    /// `rip` reached the HLE range (`Cpu::hle`): the runtime serves the call at this address.
    Hle(u64),
    /// `hlt` at this address.
    Halt(u64),
    /// `ud2` or `int3` at this address.
    Trap(u64),
    /// Division by zero or quotient overflow (`#DE`) at this address.
    DivideError(u64),
    /// A 16/32-byte SSE access that must be aligned was not (`#GP`): the address accessed.
    Misaligned(u64),
    /// An unmasked SIMD floating-point exception (`#XM`) at this address.
    FpException(u64),
    /// Another `#GP` (a reserved MXCSR bit) at this address.
    GeneralProtection(u64),
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

/// Architectural state: 16 GPRs (rax, rcx, rdx, rbx, rsp, rbp, rsi, rdi, r8..r15), rip,
/// flags, 16 xmm registers and the fs / gs bases.
#[derive(Clone)]
pub struct Cpu {
    pub gpr: [u64; 16],
    pub rip: u64,
    pub flags: u64,
    pub xmm: [u128; 16],
    /// Upper halves of the ymm registers (AVX).
    pub ymm_hi: [u128; 16],
    pub mxcsr: u32,
    pub fs_base: u64,
    pub gs_base: u64,
    /// Addresses that stop execution with `Stop::Hle` instead of being fetched.
    pub hle: Range<u64>,
    /// Instructions executed so far.
    pub steps: u64,
    icache: ICache,
}

fn mask(size: u8) -> u64 {
    if size >= 8 {
        u64::MAX
    } else {
        (1u64 << (8 * size as u32)) - 1
    }
}

fn sext(v: u64, size: u8) -> i64 {
    let shift = 64 - 8 * size as u32;
    ((v << shift) as i64) >> shift
}

fn parity(v: u64) -> bool {
    (v as u8).count_ones().is_multiple_of(2)
}

fn flag(on: bool, bit: u64) -> u64 {
    if on {
        bit
    } else {
        0
    }
}

/// SF, ZF and PF of a `size`-byte result.
fn szp(res: u64, size: u8) -> u64 {
    let res = res & mask(size);
    flag(res & (1 << (8 * size as u32 - 1)) != 0, SF) | flag(res == 0, ZF) | flag(parity(res), PF)
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
    (
        res,
        flag(cf, CF) | flag(af, AF) | flag(of, OF) | szp(res, size),
    )
}

/// Shift or rotate `a` by a non-zero, already masked `count`: result and new flags.
fn shift(op: ShiftOp, a: u64, count: u32, size: u8, flags: u64) -> (u64, u64) {
    let (m, bits) = (mask(size), 8 * size as u32);
    let a = a & m;
    let msb = |v: u64| (v >> (bits - 1)) & 1 != 0;
    let cf_in = flags & CF != 0;
    let (res, cf, of) = match op {
        ShiftOp::Shl => {
            let res = if count >= bits { 0 } else { (a << count) & m };
            let cf = count <= bits && (a >> (bits - count)) & 1 != 0;
            (res, cf, msb(res) != cf)
        }
        ShiftOp::Shr => {
            let res = if count >= bits { 0 } else { a >> count };
            let cf = count <= bits && (a >> (count - 1)) & 1 != 0;
            (res, cf, msb(a))
        }
        ShiftOp::Sar => {
            let c = count.min(bits - 1);
            let s = sext(a, size);
            let res = (s >> c) as u64 & m;
            let cf = (s >> (count.min(bits) - 1)) & 1 != 0;
            (res, cf, false)
        }
        ShiftOp::Rol | ShiftOp::Ror => {
            let c = count % bits;
            let res = if op == ShiftOp::Rol {
                ((a << c) | (a >> ((bits - c) % bits))) & m
            } else {
                ((a >> c) | (a << ((bits - c) % bits))) & m
            };
            let (cf, of) = if op == ShiftOp::Rol {
                (res & 1 != 0, msb(res) != (res & 1 != 0))
            } else {
                (msb(res), msb(res) != ((res >> (bits - 2)) & 1 != 0))
            };
            let f = (flags & !(CF | OF)) | flag(cf, CF) | flag(of, OF);
            return (res, f);
        }
        ShiftOp::Rcl | ShiftOp::Rcr => {
            let (mut v, mut c) = (a, cf_in);
            for _ in 0..count % (bits + 1) {
                if op == ShiftOp::Rcl {
                    let out = msb(v);
                    v = ((v << 1) | c as u64) & m;
                    c = out;
                } else {
                    let out = v & 1 != 0;
                    v = (v >> 1) | ((c as u64) << (bits - 1));
                    c = out;
                }
            }
            let of = if op == ShiftOp::Rcl {
                msb(v) != c
            } else {
                msb(v) != ((v >> (bits - 2)) & 1 != 0)
            };
            let f = (flags & !(CF | OF)) | flag(c, CF) | flag(of, OF);
            return (v, f);
        }
    };
    let f = (flags & !FLAG_MASK) | flag(cf, CF) | flag(of, OF) | szp(res, size);
    (res, f)
}

/// CRC-32C (Castagnoli, reflected) of the low `n` bytes of `v`, as the crc32 instruction.
fn crc32c(mut crc: u32, v: u64, n: u8) -> u32 {
    for k in 0..n {
        crc ^= (v >> (8 * k)) as u8 as u32;
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0x82f6_3b78 & (crc & 1).wrapping_neg());
        }
    }
    crc
}

impl Default for Cpu {
    /// Registers zero, MXCSR at its reset value, no HLE range.
    fn default() -> Self {
        Cpu {
            gpr: [0; 16],
            rip: 0,
            flags: 2,
            xmm: [0; 16],
            ymm_hi: [0; 16],
            mxcsr: MXCSR_RESET,
            fs_base: 0,
            gs_base: 0,
            hle: 0..0,
            steps: 0,
            icache: ICache::default(),
        }
    }
}

impl Cpu {
    /// A CPU with the given registers and an empty instruction cache.
    pub fn new(gpr: [u64; 16], rip: u64, flags: u64) -> Cpu {
        Cpu {
            gpr,
            rip,
            flags,
            ..Cpu::default()
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

    /// Effective address without the segment base (what `lea` computes).
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

    fn addr(&self, m: &MemRef, next: u64) -> u64 {
        let a = self.ea(m, next);
        match m.seg {
            Seg::None => a,
            Seg::Fs => a.wrapping_add(self.fs_base),
            Seg::Gs => a.wrapping_add(self.gs_base),
        }
    }

    fn get(&self, o: Operand, size: u8, mem: &mut dyn Mem, next: u64) -> Result<u64, Stop> {
        match o {
            Operand::Reg(r) => Ok(self.gpr[r as usize] & mask(size)),
            Operand::RegH(r) => Ok((self.gpr[r as usize] >> 8) & 0xff),
            Operand::Imm(v) => Ok(v as u64 & mask(size)),
            Operand::Xmm(x) => Ok(self.xmm[x as usize] as u64 & mask(size)),
            Operand::Mem(m) => {
                let a = self.addr(&m, next);
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
            Operand::Reg(r) => {
                let g = &mut self.gpr[r as usize];
                // A 32-bit write clears the upper half; 8- and 16-bit writes merge.
                *g = match size {
                    8 => v,
                    4 => v & 0xffff_ffff,
                    _ => (*g & !mask(size)) | (v & mask(size)),
                };
            }
            Operand::RegH(r) => {
                let g = &mut self.gpr[r as usize];
                *g = (*g & !0xff00) | ((v & 0xff) << 8);
            }
            Operand::Mem(m) => {
                let a = self.addr(&m, next);
                if !mem.write(a, size, v) {
                    return Err(Stop::Fault(a));
                }
                self.note_write(a, size);
            }
            Operand::Xmm(x) => self.xmm[x as usize] = (v & mask(size)) as u128,
            Operand::Imm(_) => unreachable!("decoder never yields an immediate destination"),
        }
        Ok(())
    }

    fn put_x(&mut self, o: Operand, v: u128, mem: &mut dyn Mem, next: u64) -> Result<(), Stop> {
        match o {
            Operand::Xmm(x) => self.xmm[x as usize] = v,
            Operand::Mem(m) => {
                let a = self.addr(&m, next);
                for (off, half) in [(0, v as u64), (8, (v >> 64) as u64)] {
                    if !mem.write(a + off, 8, half) {
                        return Err(Stop::Fault(a + off));
                    }
                }
                self.note_write(a, 16);
            }
            _ => unreachable!("decoder: xmm operand is a register or memory"),
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

    fn set_arith(&mut self, f: u64) {
        self.flags = (self.flags & !FLAG_MASK) | f;
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

    /// Branch target of a call / jmp operand: relative immediate or absolute r/m.
    fn target(&self, o: Operand, mem: &mut dyn Mem, next: u64) -> Result<u64, Stop> {
        match o {
            Operand::Imm(rel) => Ok(next.wrapping_add(rel as u64)),
            o => self.get(o, 8, mem, next),
        }
    }

    /// Execute one decoded instruction located at `rip`.
    ///
    /// # Returns
    /// `Ok(true)` when a `ret` popped `sentinel`, `Ok(false)` otherwise.
    fn exec(&mut self, i: Inst, mem: &mut dyn Mem, sentinel: u64) -> Result<bool, Stop> {
        let here = self.rip;
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
                self.set_arith(f);
                if op != AluOp::Cmp {
                    self.put(i.dst, sz, res, mem, next)?;
                }
            }
            Op::Test => {
                let (a, b) = (
                    self.get(i.dst, sz, mem, next)?,
                    self.get(i.src, sz, mem, next)?,
                );
                self.set_arith(alu(AluOp::And, a, b, false, sz).1);
            }
            Op::Mov => {
                let v = self.get(i.src, sz, mem, next)?;
                self.put(i.dst, sz, v, mem, next)?;
            }
            Op::Movzx(from) => {
                let v = self.get(i.src, from, mem, next)?;
                self.put(i.dst, sz, v, mem, next)?;
            }
            Op::Movsx(from) => {
                let v = sext(self.get(i.src, from, mem, next)?, from) as u64;
                self.put(i.dst, sz, v & mask(sz), mem, next)?;
            }
            Op::Lea => {
                let Operand::Mem(m) = i.src else {
                    unreachable!("decoder: lea source is memory")
                };
                let a = self.ea(&m, next);
                self.put(i.dst, sz, a & mask(sz), mem, next)?;
            }
            Op::Xchg => {
                let (a, b) = (
                    self.get(i.dst, sz, mem, next)?,
                    self.get(i.src, sz, mem, next)?,
                );
                self.put(i.dst, sz, b, mem, next)?;
                self.put(i.src, sz, a, mem, next)?;
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
                self.gpr[4] = self.gpr[4].wrapping_add(i.imm as u64);
                if target == sentinel {
                    return Ok(true);
                }
                self.rip = target;
            }
            Op::Call => {
                let t = self.target(i.dst, mem, next)?;
                self.push(next, mem)?;
                self.rip = t;
            }
            Op::Jmp => self.rip = self.target(i.dst, mem, next)?,
            Op::Jcc(cc) => {
                if self.cond(cc) {
                    self.rip = self.target(i.dst, mem, next)?;
                }
            }
            Op::Setcc(cc) => self.put(i.dst, 1, self.cond(cc) as u64, mem, next)?,
            Op::Cmovcc(cc) => {
                let v = self.get(i.src, sz, mem, next)?;
                // Even when not taken, a 32-bit cmov clears the upper half.
                let v = if self.cond(cc) {
                    v
                } else {
                    self.get(i.dst, sz, mem, next)?
                };
                self.put(i.dst, sz, v, mem, next)?;
            }
            Op::Leave => {
                self.gpr[4] = self.gpr[5];
                self.gpr[5] = self.pop(mem)?;
            }
            Op::Shift(op) => {
                let count = self.get(i.src, 1, mem, next)? as u32 & if sz == 8 { 63 } else { 31 };
                let a = self.get(i.dst, sz, mem, next)?;
                if count != 0 {
                    let (res, f) = shift(op, a, count, sz, self.flags);
                    self.flags = f;
                    self.put(i.dst, sz, res, mem, next)?;
                } else if sz == 4 {
                    // A zero count still writes a 32-bit register (upper half cleared).
                    self.put(i.dst, sz, a, mem, next)?;
                }
            }
            Op::Unary(op) => self.unary(op, i, mem, next, here)?,
            Op::Imul2 | Op::Imul3 => {
                let a = sext(self.get(i.src, sz, mem, next)?, sz) as i128;
                let b = if i.op == Op::Imul3 {
                    sext(i.imm as u64 & mask(sz), sz) as i128
                } else {
                    sext(self.get(i.dst, sz, mem, next)?, sz) as i128
                };
                let p = a * b;
                let res = p as u64 & mask(sz);
                let over = sext(res, sz) as i128 != p;
                self.set_arith(flag(over, CF | OF) | szp(res, sz));
                self.put(i.dst, sz, res, mem, next)?;
            }
            Op::ExtendAcc => {
                let half = sz / 2;
                let v = sext(self.gpr[0] & mask(half), half) as u64;
                self.put(Operand::Reg(0), sz, v & mask(sz), mem, next)?;
            }
            Op::SignAcc => {
                let neg = sext(self.gpr[0] & mask(sz), sz) < 0;
                self.put(
                    Operand::Reg(2),
                    sz,
                    if neg { mask(sz) } else { 0 },
                    mem,
                    next,
                )?;
            }
            Op::Bswap => {
                let v = self.gpr[i.dst_reg()];
                let r = if sz == 8 {
                    v.swap_bytes()
                } else {
                    (v as u32).swap_bytes() as u64
                };
                self.put(i.dst, sz, r, mem, next)?;
            }
            Op::Bits(op) => self.bits(op, i, mem, next)?,
            Op::Bt(op) => self.bt(op, i, mem, next)?,
            Op::Cmpxchg => {
                let acc = self.gpr[0] & mask(sz);
                let d = self.get(i.dst, sz, mem, next)?;
                self.set_arith(alu(AluOp::Cmp, acc, d, false, sz).1);
                if acc == d {
                    let s = self.get(i.src, sz, mem, next)?;
                    self.put(i.dst, sz, s, mem, next)?;
                } else {
                    self.put(Operand::Reg(0), sz, d, mem, next)?;
                }
            }
            Op::Xadd => {
                let (d, s) = (
                    self.get(i.dst, sz, mem, next)?,
                    self.get(i.src, sz, mem, next)?,
                );
                let (sum, f) = alu(AluOp::Add, d, s, false, sz);
                self.set_arith(f);
                self.put(i.src, sz, d, mem, next)?;
                self.put(i.dst, sz, sum, mem, next)?;
            }
            Op::Movs | Op::Stos => self.string(i, mem)?,
            Op::Cld => self.flags &= !DF,
            Op::Std => self.flags |= DF,
            Op::Hlt => return Err(Stop::Halt(here)),
            Op::Trap => return Err(Stop::Trap(here)),
            Op::Sse(op) => self.sse(op, i, mem, next)?,
            Op::Bmi(op) => self.bmi(op, i, mem, next)?,
            Op::Movbe => {
                let v = self.get(i.src, sz, mem, next)?;
                let swapped = match sz {
                    2 => (v as u16).swap_bytes() as u64,
                    4 => (v as u32).swap_bytes() as u64,
                    _ => v.swap_bytes(),
                };
                self.put(i.dst, sz, swapped, mem, next)?;
            }
            Op::Mxcsr(load) => self.mxcsr_op(load, i, mem, next)?,
            Op::Crc32(from) => {
                let v = self.get(i.src, from, mem, next)?;
                let crc = crc32c(self.gpr[i.dst_reg()] as u32, v, from);
                // The accumulator is 32-bit; a 64-bit destination gets it zero-extended.
                self.put(i.dst, 4, crc as u64, mem, next)?;
            }
        }
        Ok(false)
    }

    #[inline(never)]
    fn unary(
        &mut self,
        op: UnOp,
        i: Inst,
        mem: &mut dyn Mem,
        next: u64,
        here: u64,
    ) -> Result<(), Stop> {
        let sz = i.size;
        let v = self.get(i.dst, sz, mem, next)?;
        match op {
            UnOp::Not => self.put(i.dst, sz, !v, mem, next)?,
            UnOp::Neg => {
                let (res, f) = alu(AluOp::Sub, 0, v, false, sz);
                self.set_arith(f);
                self.put(i.dst, sz, res, mem, next)?;
            }
            UnOp::Inc | UnOp::Dec => {
                let op = if op == UnOp::Inc {
                    AluOp::Add
                } else {
                    AluOp::Sub
                };
                let (res, f) = alu(op, v, 1, false, sz);
                self.set_arith((f & !CF) | (self.flags & CF));
                self.put(i.dst, sz, res, mem, next)?;
            }
            UnOp::Mul | UnOp::Imul => {
                let bits = 8 * sz as u32;
                let acc = self.gpr[0] & mask(sz);
                let (lo, hi, over) = if op == UnOp::Mul {
                    let p = acc as u128 * v as u128;
                    (
                        p as u64 & mask(sz),
                        (p >> bits) as u64 & mask(sz),
                        p >> bits != 0,
                    )
                } else {
                    let p = sext(acc, sz) as i128 * sext(v, sz) as i128;
                    let lo = p as u64 & mask(sz);
                    (lo, (p >> bits) as u64 & mask(sz), sext(lo, sz) as i128 != p)
                };
                self.set_arith(flag(over, CF | OF) | szp(lo, sz));
                self.store_pair(sz, lo, hi, mem, next)?;
            }
            UnOp::Div | UnOp::Idiv => {
                let bits = 8 * sz as u32;
                let (lo, hi) = if sz == 1 {
                    (self.gpr[0] & 0xff, (self.gpr[0] >> 8) & 0xff)
                } else {
                    (self.gpr[0] & mask(sz), self.gpr[2] & mask(sz))
                };
                let n = (hi as u128) << bits | lo as u128;
                if v == 0 {
                    return Err(Stop::DivideError(here));
                }
                let (q, r) = if op == UnOp::Div {
                    let (q, r) = (n / v as u128, n % v as u128);
                    if q > mask(sz) as u128 {
                        return Err(Stop::DivideError(here));
                    }
                    (q as u64, r as u64)
                } else {
                    let n = ((n << (128 - 2 * bits)) as i128) >> (128 - 2 * bits);
                    let d = sext(v, sz) as i128;
                    let (q, r) = (n.wrapping_div(d), n.wrapping_rem(d));
                    if sext(q as u64 & mask(sz), sz) as i128 != q {
                        return Err(Stop::DivideError(here));
                    }
                    (q as u64 & mask(sz), r as u64 & mask(sz))
                };
                self.store_pair(sz, q, r, mem, next)?;
            }
        }
        Ok(())
    }

    /// Write a mul / div result pair: al:ah for bytes, else rax and rdx.
    fn store_pair(
        &mut self,
        sz: u8,
        a: u64,
        d: u64,
        mem: &mut dyn Mem,
        next: u64,
    ) -> Result<(), Stop> {
        if sz == 1 {
            self.put(Operand::Reg(0), 2, a | (d << 8), mem, next)
        } else {
            self.put(Operand::Reg(0), sz, a, mem, next)?;
            self.put(Operand::Reg(2), sz, d, mem, next)
        }
    }

    #[inline(never)]
    fn bits(&mut self, op: BitOp, i: Inst, mem: &mut dyn Mem, next: u64) -> Result<(), Stop> {
        let sz = i.size;
        let bits = 8 * sz as u32;
        let v = self.get(i.src, sz, mem, next)?;
        let lz = v.leading_zeros() - (64 - bits);
        let (res, f) = match op {
            BitOp::Bsf | BitOp::Bsr if v == 0 => {
                // The destination is left as it was.
                self.set_arith(ZF);
                return Ok(());
            }
            BitOp::Bsf => (v.trailing_zeros() as u64, 0),
            BitOp::Bsr => ((bits - 1 - lz) as u64, 0),
            BitOp::Tzcnt => {
                let r = if v == 0 { bits } else { v.trailing_zeros() };
                (r as u64, flag(v == 0, CF) | flag(r == 0, ZF))
            }
            BitOp::Lzcnt => (lz as u64, flag(v == 0, CF) | flag(lz == 0, ZF)),
            BitOp::Popcnt => (v.count_ones() as u64, flag(v == 0, ZF)),
        };
        self.set_arith(f);
        self.put(i.dst, sz, res, mem, next)
    }

    #[inline(never)]
    fn bt(&mut self, op: BtOp, i: Inst, mem: &mut dyn Mem, next: u64) -> Result<(), Stop> {
        let sz = i.size;
        let bits = 8 * sz as i64;
        let (dst, bit) = match (i.dst, i.src) {
            (d, Operand::Imm(n)) => (d, n & (bits - 1)),
            (Operand::Mem(mut m), s) => {
                // A register offset addresses a bit string: it may leave the operand.
                let off = sext(self.get(s, sz, mem, next)?, sz);
                m.disp = m.disp.wrapping_add(off.div_euclid(bits) * sz as i64);
                (Operand::Mem(m), off.rem_euclid(bits))
            }
            (d, s) => (d, self.get(s, sz, mem, next)? as i64 & (bits - 1)),
        };
        let v = self.get(dst, sz, mem, next)?;
        let b = 1u64 << bit;
        self.flags = (self.flags & !CF) | flag(v & b != 0, CF);
        let res = match op {
            BtOp::Bt => return Ok(()),
            BtOp::Bts => v | b,
            BtOp::Btr => v & !b,
            BtOp::Btc => v ^ b,
        };
        self.put(dst, sz, res, mem, next)
    }

    /// movs / stos, with or without a rep prefix (64-bit addresses).
    #[inline(never)]
    fn string(&mut self, i: Inst, mem: &mut dyn Mem) -> Result<(), Stop> {
        let sz = i.size;
        let step = if self.flags & DF != 0 {
            (sz as u64).wrapping_neg()
        } else {
            sz as u64
        };
        let rep = i.rep != Rep::None;
        while !rep || self.gpr[1] != 0 {
            let v = if i.op == Op::Movs {
                let s = self.gpr[6];
                let v = mem.read(s, sz).ok_or(Stop::Fault(s))?;
                self.gpr[6] = s.wrapping_add(step);
                v
            } else {
                self.gpr[0] & mask(sz)
            };
            let d = self.gpr[7];
            if !mem.write(d, sz, v) {
                return Err(Stop::Fault(d));
            }
            self.note_write(d, sz);
            self.gpr[7] = d.wrapping_add(step);
            if !rep {
                break;
            }
            self.gpr[1] -= 1;
        }
        Ok(())
    }

    #[inline(never)]
    fn bmi(&mut self, op: BmiOp, i: Inst, mem: &mut dyn Mem, next: u64) -> Result<(), Stop> {
        let sz = i.size;
        let (m, bits) = (mask(sz), 8 * sz as u64);
        let src = self.get(i.src, sz, mem, next)?;
        let other = self.gpr[i.vvvv as usize] & m;
        let sign = |r: u64| flag(r & (1 << (bits - 1)) != 0, SF);
        let (res, f) = match op {
            BmiOp::Andn => {
                let r = !other & src & m;
                (r, sign(r) | flag(r == 0, ZF))
            }
            BmiOp::Bextr => {
                let (start, len) = (other & 0xff, (other >> 8) & 0xff);
                let r = if start >= bits {
                    0
                } else {
                    let v = src >> start;
                    if len >= 64 {
                        v
                    } else {
                        v & ((1u64 << len) - 1)
                    }
                };
                (r, flag(r == 0, ZF))
            }
            BmiOp::Blsr => {
                let r = src & src.wrapping_sub(1) & m;
                (r, sign(r) | flag(r == 0, ZF) | flag(src == 0, CF))
            }
            BmiOp::Blsmsk => {
                let r = (src ^ src.wrapping_sub(1)) & m;
                (r, sign(r) | flag(src == 0, CF))
            }
            BmiOp::Blsi => {
                let r = src.isolate_lowest_one() & m;
                (r, sign(r) | flag(r == 0, ZF) | flag(src != 0, CF))
            }
        };
        self.set_arith(f);
        self.put(i.dst, sz, res, mem, next)
    }

    /// Run from `self.rip` until a `ret` pops `sentinel`.
    ///
    /// # Args
    /// * `mem` - guest memory; code is read from it too
    /// * `sentinel` - return address that ends the run
    /// * `max_steps` - instruction budget (guards against infinite loops)
    ///
    /// # Returns
    /// `Ok(())` once the sentinel is returned to, otherwise why it stopped (`Stop::Hle`
    /// when `rip` enters `self.hle`: the caller serves the call and runs again).
    pub fn run(&mut self, mem: &mut dyn Mem, sentinel: u64, max_steps: u64) -> Result<(), Stop> {
        for _ in 0..max_steps {
            if self.hle.contains(&self.rip) {
                return Err(Stop::Hle(self.rip));
            }
            let inst = self.fetch(mem)?;
            self.steps += 1;
            if self.exec(inst, mem, sentinel)? {
                return Ok(());
            }
        }
        Err(Stop::Budget)
    }
}

impl Inst {
    fn dst_reg(&self) -> usize {
        match self.dst {
            Operand::Reg(r) => r as usize,
            _ => unreachable!("decoder: register destination"),
        }
    }
}
