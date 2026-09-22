//! xHCI host controller: command, event and transfer rings, device
//! contexts, control, interrupt and bulk transfers.

use alloc::collections::VecDeque;
use alloc::vec::Vec;
use core::sync::atomic::{fence, Ordering};

use libnx::abi::err::{EIO, ENOMEM, ETIMEDOUT};
use libnx::abi::shm::{CONTIGUOUS, DMA32, UNCACHED};
use libnx::drv::{Irq, Mmio};
use libnx::mem::SharedMemory;
use libnx::time;

pub type Res<T> = Result<T, i64>;

// TRB types.
const TRB_NORMAL: u32 = 1;
const TRB_SETUP: u32 = 2;
const TRB_DATA: u32 = 3;
const TRB_STATUS: u32 = 4;
const TRB_LINK: u32 = 6;
const TRB_ENABLE_SLOT: u32 = 9;
const TRB_ADDRESS_DEVICE: u32 = 11;
const TRB_CONFIGURE_EP: u32 = 12;
const TRB_EVALUATE_CTX: u32 = 13;
pub const EV_TRANSFER: u32 = 32;
const EV_COMMAND: u32 = 33;

const CC_SUCCESS: u32 = 1;
pub const CC_SHORT: u32 = 13;

pub fn trb_type(t: &[u32; 4]) -> u32 {
    (t[3] >> 10) & 0x3F
}

pub fn completion(t: &[u32; 4]) -> u32 {
    t[2] >> 24
}

pub fn trb_ptr(t: &[u32; 4]) -> u64 {
    t[0] as u64 | (t[1] as u64) << 32
}

/// Controller DMA memory: contiguous, uncached, below 4 GiB; handed out
/// by a simple bump pointer (nothing is freed).
pub struct Dma {
    shm: SharedMemory,
    pa: u64,
    next: usize,
}

impl Dma {
    pub fn new(len: usize) -> Res<Self> {
        let shm = SharedMemory::new(len, CONTIGUOUS | UNCACHED | DMA32)?;
        let pa = shm.paddr(0)?;
        Ok(Self { shm, pa, next: 0 })
    }

    /// Zeroed `len`-byte area aligned on `align`: (virtual, physical).
    pub fn alloc(&mut self, len: usize, align: usize) -> Res<(*mut u8, u64)> {
        let off = self.next.next_multiple_of(align);
        if off + len > self.shm.len() {
            return Err(-ENOMEM);
        }
        self.next = off + len;
        let va = unsafe { self.shm.as_ptr().add(off) };
        unsafe { core::ptr::write_bytes(va, 0, len) };
        Ok((va, self.pa + off as u64))
    }
}

/// TRB ring (commands or transfers), with its final link TRB.
pub struct Ring {
    base: *mut u32,
    pub pa: u64,
    n: usize,
    enq: usize,
    cycle: u32,
}

impl Ring {
    pub fn new(dma: &mut Dma, n: usize) -> Res<Self> {
        let (va, pa) = dma.alloc(n * 16, 64)?;
        let r = Self {
            base: va as *mut u32,
            pa,
            n,
            enq: 0,
            cycle: 1,
        };
        // Link back to the start, toggling the cycle bit.
        r.write(n - 1, pa, 0, (TRB_LINK << 10) | (1 << 1));
        Ok(r)
    }

    fn write(&self, i: usize, param: u64, status: u32, control: u32) {
        unsafe {
            let p = self.base.add(i * 4);
            p.write_volatile(param as u32);
            p.add(1).write_volatile((param >> 32) as u32);
            p.add(2).write_volatile(status);
            fence(Ordering::Release);
            p.add(3).write_volatile(control);
        }
    }

    /// Push a TRB (cycle bit set here); return its address.
    pub fn push(&mut self, param: u64, status: u32, control: u32) -> u64 {
        let i = self.enq;
        self.write(i, param, status, control | self.cycle);
        let pa = self.pa + i as u64 * 16;
        self.enq += 1;
        if self.enq == self.n - 1 {
            let link = unsafe { self.base.add((self.n - 1) * 4 + 3).read_volatile() };
            unsafe {
                self.base
                    .add((self.n - 1) * 4 + 3)
                    .write_volatile((link & !1) | self.cycle)
            };
            self.enq = 0;
            self.cycle ^= 1;
        }
        pa
    }
}

struct EventRing {
    base: *mut u32,
    pa: u64,
    n: usize,
    deq: usize,
    cycle: u32,
}

impl EventRing {
    fn pop(&mut self) -> Option<[u32; 4]> {
        let p = unsafe { self.base.add(self.deq * 4) };
        let control = unsafe { p.add(3).read_volatile() };
        if control & 1 != self.cycle {
            return None;
        }
        fence(Ordering::Acquire);
        let t = unsafe {
            [
                p.read_volatile(),
                p.add(1).read_volatile(),
                p.add(2).read_volatile(),
                control,
            ]
        };
        self.deq += 1;
        if self.deq == self.n {
            self.deq = 0;
            self.cycle ^= 1;
        }
        Some(t)
    }

    fn deq_pa(&self) -> u64 {
        self.pa + self.deq as u64 * 16
    }
}

/// An addressed device.
pub struct Device {
    pub slot: u8,
    #[allow(dead_code)]
    pub port: u8,
    pub speed: u8,
    input: (*mut u8, u64),
    ep0: Ring,
    pub rings: Vec<Option<Ring>>,
    /// Control-transfer buffer (4 KiB).
    buf: (*mut u8, u64),
}

/// Endpoint to configure: USB address, xHCI type, size, interval.
#[derive(Clone, Copy)]
pub struct EpSpec {
    pub address: u8,
    pub kind: u32,
    pub max_packet: u16,
    pub interval: u8,
}

/// DCI (context index) of a USB endpoint.
pub fn dci(address: u8) -> usize {
    (address as usize & 0xF) * 2 + usize::from(address & 0x80 != 0)
}

pub struct Xhci {
    regs: Mmio,
    op: usize,
    rt: usize,
    db: usize,
    pub ports: u32,
    ctx: usize,
    pub dma: Dma,
    dcbaa: *mut u64,
    cmd: Ring,
    evt: EventRing,
    pub irq: Irq,
    /// Events received while waiting for something else.
    pub pending: VecDeque<[u32; 4]>,
}

impl Xhci {
    pub fn new(regs: Mmio, irq: Irq) -> Res<Self> {
        let caplen = regs.r8(0) as usize;
        let hcs1 = regs.r32(0x04);
        let hcc1 = regs.r32(0x10);
        let (db, rt) = (
            (regs.r32(0x14) & !3) as usize,
            (regs.r32(0x18) & !0x1F) as usize,
        );
        let slots = (hcs1 & 0xFF).min(16);
        let ports = hcs1 >> 24;
        let ctx = if hcc1 & (1 << 2) != 0 { 64 } else { 32 };
        let op = caplen;

        // Halt then reset the controller.
        regs.w32(op, regs.r32(op) & !1);
        regs.wait(op + 4, 1, 1, 100_000);
        regs.w32(op, regs.r32(op) | 2);
        if !regs.wait(op, 2, 0, 1_000_000) || !regs.wait(op + 4, 1 << 11, 0, 1_000_000) {
            return Err(-ETIMEDOUT);
        }

        let mut dma = Dma::new(1 << 20)?;
        let (dcbaa, dcbaa_pa) = dma.alloc((slots as usize + 1) * 8, 64)?;
        let cmd = Ring::new(&mut dma, 64)?;
        let (ev, ev_pa) = dma.alloc(128 * 16, 64)?;
        let (erst, erst_pa) = dma.alloc(16, 64)?;
        unsafe {
            (erst as *mut u64).write_volatile(ev_pa);
            (erst as *mut u32).add(2).write_volatile(128);
        }
        regs.w32(op + 0x38, slots); // CONFIG.MaxSlotsEn
        regs.w32(op + 0x30, dcbaa_pa as u32);
        regs.w32(op + 0x34, (dcbaa_pa >> 32) as u32);
        regs.w32(op + 0x18, cmd.pa as u32 | 1);
        regs.w32(op + 0x1C, (cmd.pa >> 32) as u32);
        let ir = rt + 0x20;
        regs.w32(ir + 0x08, 1); // ERSTSZ
        regs.w32(ir + 0x18, ev_pa as u32);
        regs.w32(ir + 0x1C, (ev_pa >> 32) as u32);
        regs.w32(ir + 0x10, erst_pa as u32);
        regs.w32(ir + 0x14, (erst_pa >> 32) as u32);
        regs.w32(ir + 0x04, 0); // no moderation
        regs.w32(ir, 3); // IMAN: IE, ack IP
        regs.w32(op, regs.r32(op) | 1 | 4); // Run, INTE
        if !regs.wait(op + 4, 1, 0, 100_000) {
            return Err(-ETIMEDOUT);
        }
        Ok(Self {
            regs,
            op,
            rt,
            db,
            ports,
            ctx,
            dma,
            dcbaa: dcbaa as *mut u64,
            cmd,
            evt: EventRing {
                base: ev as *mut u32,
                pa: ev_pa,
                n: 128,
                deq: 0,
                cycle: 1,
            },
            irq,
            pending: VecDeque::new(),
        })
    }

    pub fn ring_doorbell(&self, slot: u8, target: u32) {
        fence(Ordering::SeqCst);
        self.regs.w32(self.db + slot as usize * 4, target);
    }

    /// Collect available events (without waiting) into `pending`.
    pub fn collect(&mut self) {
        let mut any = false;
        while let Some(t) = self.evt.pop() {
            self.pending.push_back(t);
            any = true;
        }
        if any {
            let ir = self.rt + 0x20;
            let d = self.evt.deq_pa();
            self.regs.w32(ir + 0x18, d as u32 | 1 << 3); // ERDP, clear EHB
            self.regs.w32(ir + 0x1C, (d >> 32) as u32);
        }
        self.regs.w32(self.op + 4, 1 << 3); // USBSTS.EINT
        self.regs.w32(self.rt + 0x20, 3); // IMAN.IP
    }

    /// Wait for an event matching `pred`; others stay queued.
    fn wait_event(&mut self, pred: impl Fn(&[u32; 4]) -> bool, timeout_ms: u64) -> Res<[u32; 4]> {
        let end = time::now_ns() + timeout_ms * 1_000_000;
        loop {
            self.collect();
            if let Some(k) = self.pending.iter().position(&pred) {
                return Ok(self.pending.remove(k).unwrap());
            }
            let now = time::now_ns();
            if now > end {
                return Err(-ETIMEDOUT);
            }
            self.irq.wait((end - now).min(2_000_000));
            self.irq.ack();
        }
    }

    fn command(&mut self, param: u64, control: u32) -> Res<[u32; 4]> {
        let at = self.cmd.push(param, 0, control);
        self.ring_doorbell(0, 0);
        let ev = self.wait_event(|t| trb_type(t) == EV_COMMAND && trb_ptr(t) == at, 1000)?;
        if completion(&ev) != CC_SUCCESS {
            return Err(-EIO);
        }
        Ok(ev)
    }

    /// Reset a connected port; return its speed (xHCI encoding).
    pub fn reset_port(&mut self, port: u32) -> Option<u8> {
        let reg = self.op + 0x400 + 0x10 * (port as usize - 1);
        let ps = self.regs.r32(reg);
        if ps & 1 == 0 {
            return None;
        }
        // Bits to keep as-is (read-only and read/write).
        let neutral = |v: u32| {
            v & ((1 | 1 << 3 | 0xF << 10 | 1 << 30) | (0xF << 5 | 1 << 9 | 3 << 14 | 7 << 25))
        };
        self.regs.w32(reg, neutral(ps) | 1 << 4); // PR
        let end = time::now_ns() + 500_000_000;
        while self.regs.r32(reg) & (1 << 21) == 0 && time::now_ns() < end {
            time::sleep_ms(1);
        }
        let ps = self.regs.r32(reg);
        self.regs.w32(reg, neutral(ps) | 1 << 21 | 1 << 17); // clear PRC, CSC
        if ps & 2 == 0 {
            return None; // not active
        }
        Some(((ps >> 10) & 0xF) as u8)
    }

    fn ctx_ptr(&self, base: *mut u8, index: usize) -> *mut u32 {
        unsafe { base.add(index * self.ctx) as *mut u32 }
    }

    /// New device on `port`: slot, contexts, address.
    pub fn address(&mut self, port: u32, speed: u8) -> Res<Device> {
        let ev = self.command(0, TRB_ENABLE_SLOT << 10)?;
        let slot = (ev[3] >> 24) as u8;
        let (out, out_pa) = self.dma.alloc(32 * self.ctx, 64)?;
        let _ = out;
        let input = self.dma.alloc(33 * self.ctx, 64)?;
        let ep0 = Ring::new(&mut self.dma, 32)?;
        let buf = self.dma.alloc(4096, 64)?;
        unsafe { self.dcbaa.add(slot as usize).write_volatile(out_pa) };
        let mps: u32 = match speed {
            2 => 8,   // low speed
            4 => 512, // SuperSpeed
            _ => 64,
        };
        unsafe {
            let icc = self.ctx_ptr(input.0, 0);
            icc.add(1).write_volatile(0b11); // A0 (slot) | A1 (EP0)
            let sc = self.ctx_ptr(input.0, 1);
            sc.write_volatile((1 << 27) | (speed as u32) << 20);
            sc.add(1).write_volatile(port << 16);
            let ep = self.ctx_ptr(input.0, 2);
            ep.add(1).write_volatile((mps << 16) | (4 << 3) | (3 << 1)); // control, CErr 3
            ep.add(2).write_volatile(ep0.pa as u32 | 1);
            ep.add(3).write_volatile((ep0.pa >> 32) as u32);
            ep.add(4).write_volatile(8); // average TRB length
        }
        self.command(input.1, (TRB_ADDRESS_DEVICE << 10) | (slot as u32) << 24)?;
        let mut rings = Vec::new();
        rings.resize_with(32, || None);
        Ok(Device {
            slot,
            port: port as u8,
            speed,
            input,
            ep0,
            rings,
            buf,
        })
    }

    /// Control transfer; `data` is read (IN) or written (OUT).
    pub fn control(
        &mut self,
        dev: &mut Device,
        req_type: u8,
        req: u8,
        value: u16,
        index: u16,
        data: &mut [u8],
    ) -> Res<usize> {
        let len = data.len().min(4096);
        let setup = req_type as u64
            | (req as u64) << 8
            | (value as u64) << 16
            | (index as u64) << 32
            | (len as u64) << 48;
        let in_dir = req_type & 0x80 != 0;
        let trt = if len == 0 {
            0
        } else if in_dir {
            3
        } else {
            2
        };
        if !in_dir && len > 0 {
            unsafe { core::ptr::copy_nonoverlapping(data.as_ptr(), dev.buf.0, len) };
        }
        dev.ep0
            .push(setup, 8, (TRB_SETUP << 10) | (1 << 6) | (trt << 16));
        if len > 0 {
            dev.ep0.push(
                dev.buf.1,
                len as u32,
                (TRB_DATA << 10) | u32::from(in_dir) << 16,
            );
        }
        let status_in = len == 0 || !in_dir;
        let at = dev.ep0.push(
            0,
            0,
            (TRB_STATUS << 10) | (1 << 5) | u32::from(status_in) << 16,
        );
        self.ring_doorbell(dev.slot, 1);
        let slot = dev.slot;
        let ev = self.wait_event(
            |t| {
                trb_type(t) == EV_TRANSFER
                    && (t[3] >> 24) as u8 == slot
                    && (trb_ptr(t) == at || completion(t) != CC_SUCCESS)
            },
            1000,
        )?;
        let cc = completion(&ev);
        if cc != CC_SUCCESS && cc != CC_SHORT {
            return Err(-EIO);
        }
        if in_dir && len > 0 {
            unsafe { core::ptr::copy_nonoverlapping(dev.buf.0, data.as_mut_ptr(), len) };
        }
        Ok(len)
    }

    /// Set endpoint 0 packet size (read from the descriptor).
    pub fn set_ep0_packet(&mut self, dev: &mut Device, mps: u16) -> Res<()> {
        unsafe {
            let icc = self.ctx_ptr(dev.input.0, 0);
            icc.write_volatile(0);
            icc.add(1).write_volatile(0b10); // A1
            let ep = self.ctx_ptr(dev.input.0, 2);
            let d1 = ep.add(1).read_volatile();
            ep.add(1).write_volatile((d1 & 0xFFFF) | (mps as u32) << 16);
        }
        self.command(
            dev.input.1,
            (TRB_EVALUATE_CTX << 10) | (dev.slot as u32) << 24,
        )
        .map(|_| ())
    }

    /// Configure endpoints (fresh rings) after SET_CONFIGURATION.
    pub fn configure(&mut self, dev: &mut Device, eps: &[EpSpec]) -> Res<()> {
        let mut add = 1u32; // slot context
        let mut last = 1;
        for e in eps {
            let d = dci(e.address);
            let ring = Ring::new(&mut self.dma, 64)?;
            unsafe {
                let ep = self.ctx_ptr(dev.input.0, d + 1);
                for k in 0..8 {
                    ep.add(k).write_volatile(0);
                }
                // xHCI interval: exponent in 125 us microframes.
                let interval = match (e.kind, dev.speed) {
                    (3 | 7, 1 | 2) => (e.interval.max(1) as u32 * 8).ilog2(),
                    (3 | 7, _) => e.interval.clamp(1, 16) as u32 - 1,
                    _ => 0,
                };
                ep.write_volatile(interval << 16);
                ep.add(1)
                    .write_volatile((e.max_packet as u32) << 16 | e.kind << 3 | 3 << 1);
                ep.add(2).write_volatile(ring.pa as u32 | 1);
                ep.add(3).write_volatile((ring.pa >> 32) as u32);
                ep.add(4)
                    .write_volatile(if e.kind == 7 || e.kind == 3 { 8 } else { 1024 });
            }
            dev.rings[d] = Some(ring);
            add |= 1 << d;
            last = last.max(d);
        }
        unsafe {
            let icc = self.ctx_ptr(dev.input.0, 0);
            icc.write_volatile(0);
            icc.add(1).write_volatile(add);
            let sc = self.ctx_ptr(dev.input.0, 1);
            let d0 = sc.read_volatile();
            sc.write_volatile((d0 & !(0x1F << 27)) | (last as u32) << 27);
        }
        self.command(
            dev.input.1,
            (TRB_CONFIGURE_EP << 10) | (dev.slot as u32) << 24,
        )
        .map(|_| ())
    }

    /// Queue a transfer (Normal TRB) on endpoint `d`.
    pub fn queue(&mut self, dev: &mut Device, d: usize, pa: u64, len: u32) -> Res<u64> {
        let ring = dev.rings[d].as_mut().ok_or(-EIO)?;
        let at = ring.push(pa, len, (TRB_NORMAL << 10) | (1 << 5) | (1 << 2)); // IOC, ISP
        self.ring_doorbell(dev.slot, d as u32);
        Ok(at)
    }
}
