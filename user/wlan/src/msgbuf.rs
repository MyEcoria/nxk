//! Broadcom msgbuf protocol over PCIe (Linux `brcmfmac`: `pcie.c`,
//! `commonring.c`, `msgbuf.c`): rings in host memory, their indices in
//! the chip's RAM, doorbells through the PCIe2 core's mailbox.

use alloc::collections::VecDeque;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use libnx::abi::shm::{CONTIGUOUS, DMA32, UNCACHED};
use libnx::drv::Mmio;
use libnx::mem::SharedMemory;
use libnx::time;

use crate::chip::{Backplane, Core};
use crate::fw::Up;

// Shared area fields (offsets from its base).
const SHARED_DMA_INDEX: u32 = 0x1_0000;
const SHARED_RX_DATAOFFSET: u32 = 36;
const SHARED_RING_INFO: u32 = 48;
const SHARED_SCRATCH_LEN: u32 = 52;
const SHARED_SCRATCH_ADDR: u32 = 56;
const SHARED_RINGUPD_LEN: u32 = 64;
const SHARED_RINGUPD_ADDR: u32 = 68;
const SCRATCH_LEN: usize = 8;
const RINGUPD_LEN: usize = 1024;

// Ring info: ring descriptors, then where each index lives in TCM.
const RINGINFO_RINGMEM: u32 = 0;
const RINGINFO_H2D_W: u32 = 4;
const RINGINFO_H2D_R: u32 = 8;
const RINGINFO_D2H_W: u32 = 12;
const RINGINFO_D2H_R: u32 = 16;
/// Ring descriptor in TCM: item count, item size, then its host address.
const RING_MEM_SZ: u32 = 16;
const RING_MAX_ITEM: u32 = 4;
const RING_LEN_ITEMS: u32 = 6;
const RING_BASE_ADDR: u32 = 8;

/// Common rings: two host-to-dongle, three back. Shared version 5 has
/// the pre-v7 completion sizes.
const CONTROL_SUBMIT: usize = 0;
const CONTROL_COMPLETE: usize = 2;
const RINGS: [(u16, u16); 5] = [(64, 40), (512, 32), (64, 24), (1024, 16), (512, 32)];

// PCIe2 core mailbox (core rev < 64).
const PCIE2_H2D_MAILBOX_0: u32 = 0x140;

// Message types.
const MSG_IOCTLPTR_REQ: u8 = 0x9;
const MSG_IOCTLRESP_BUF_POST: u8 = 0xB;
const MSG_IOCTL_CMPLT: u8 = 0xC;
const MSG_EVENT_BUF_POST: u8 = 0xD;
const MSG_WL_EVENT: u8 = 0xE;
/// Request id of the one ioctl in flight (`BRCMF_IOCTL_REQ_PKTID`).
const IOCTL_REQ_PKTID: u32 = 0xFFFE;
/// Control buffers: responses, then events (`BRCMF_MSGBUF_MAX_*_POST`).
const CTL_BUFS: usize = 8;
const CTL_BUF_LEN: usize = 8192;
/// Largest ioctl payload sent (`BRCMF_TX_IOCTL_MAX_MSG_SIZE`).
const IOCTL_MAX: usize = 1518;
const IOCTL_TIMEOUT_MS: u64 = 2000;
/// Events kept for a reader; older ones are dropped past this.
const EVENTS_MAX: usize = 256;
/// D2H completions carry a modulo-253 sequence number (flag 0x20000).
const EPOCH_MODULO: u32 = 253;

pub const WLC_GET_VAR: u32 = 262;
pub const WLC_SET_VAR: u32 = 263;

/// Contiguous uncached memory handed out by a bump pointer.
struct Dma {
    shm: SharedMemory,
    pa: u64,
    next: usize,
}

impl Dma {
    fn alloc(&mut self, len: usize, align: usize) -> Result<(*mut u8, u32), String> {
        let off = self.next.next_multiple_of(align);
        if off + len > self.shm.len() {
            return Err(format!("DMA area full ({} bytes)", self.shm.len()));
        }
        self.next = off + len;
        let va = unsafe { self.shm.as_ptr().add(off) };
        unsafe { core::ptr::write_bytes(va, 0, len) };
        Ok((va, (self.pa + off as u64) as u32))
    }
}

/// One ring: items in host memory, read and write indices in TCM.
struct Ring {
    va: *mut u8,
    items: u16,
    size: u16,
    w_idx: u32,
    r_idx: u32,
    /// Host side: next item to write (H2D) or to read (D2H).
    pos: u16,
    /// Expected D2H sequence number, once the first one is seen.
    epoch: Option<u32>,
}

impl Ring {
    fn item(&self, k: u16) -> *mut u8 {
        unsafe { self.va.add(k as usize * self.size as usize) }
    }
}

/// Barrier before a doorbell: ring contents reach memory first.
fn dma_wmb() {
    unsafe { core::arch::asm!("dsb st", options(nostack, preserves_flags)) };
}

fn put16(p: *mut u8, off: usize, v: u16) {
    unsafe { core::ptr::copy_nonoverlapping(v.to_le_bytes().as_ptr(), p.add(off), 2) };
}

fn put32(p: *mut u8, off: usize, v: u32) {
    unsafe { core::ptr::copy_nonoverlapping(v.to_le_bytes().as_ptr(), p.add(off), 4) };
}

fn get16(p: *const u8, off: usize) -> u16 {
    let mut b = [0u8; 2];
    unsafe { core::ptr::copy_nonoverlapping(p.add(off), b.as_mut_ptr(), 2) };
    u16::from_le_bytes(b)
}

fn get32(p: *const u8, off: usize) -> u32 {
    let mut b = [0u8; 4];
    unsafe { core::ptr::copy_nonoverlapping(p.add(off), b.as_mut_ptr(), 4) };
    u32::from_le_bytes(b)
}

pub struct Msgbuf {
    _dma: Dma,
    rings: Vec<Ring>,
    pcie2: Core,
    ioctl_buf: (*mut u8, u32),
    /// Posted control buffers by request id: responses 0..8, events 8..16.
    ctl: Vec<(*mut u8, u32)>,
    trans_id: u16,
    /// Where frames start in a buffer the firmware filled.
    rx_dataoffset: u32,
    /// Firmware event frames (Ethernet, Broadcom ethertype), oldest first.
    events: VecDeque<Vec<u8>>,
    reply: Option<Reply>,
    /// The firmware console, for `console_tail`.
    pub console: u32,
}

/// Outcome of an ioctl: firmware status and the bytes it returned.
pub struct Reply {
    pub status: i16,
    pub data: Vec<u8>,
}

impl Msgbuf {
    /// Rings and buffers set up for the running firmware
    /// (`brcmf_pcie_init_ringbuffers`, `_init_scratchbuffers`, then the
    /// control buffers `brcmf_proto_msgbuf_attach` posts).
    pub fn attach(bp: &Backplane, tcm: &Mmio, pcie2: Core, up: &Up) -> Result<Self, String> {
        if up.flags & SHARED_DMA_INDEX != 0 {
            return Err(format!(
                "firmware wants DMA indices (flags {:#x})",
                up.flags
            ));
        }
        let t = |a: u32| tcm.r32(a as usize);
        let shm = SharedMemory::new(256 * 1024, CONTIGUOUS | UNCACHED | DMA32)
            .map_err(|e| format!("DMA memory ({})", libnx::abi::err::name(e)))?;
        let pa = shm
            .paddr(0)
            .map_err(|e| format!("DMA address ({})", libnx::abi::err::name(e)))?;
        let mut dma = Dma { shm, pa, next: 0 };

        let info = t(up.shared + SHARED_RING_INFO);
        let mut mem = t(info + RINGINFO_RINGMEM);
        let (mut h2d_w, mut h2d_r) = (t(info + RINGINFO_H2D_W), t(info + RINGINFO_H2D_R));
        let (mut d2h_w, mut d2h_r) = (t(info + RINGINFO_D2H_W), t(info + RINGINFO_D2H_R));
        let mut rings = Vec::new();
        for (k, &(items, size)) in RINGS.iter().enumerate() {
            let (va, ring_pa) = dma.alloc(items as usize * size as usize, 8)?;
            tcm.w32((mem + RING_BASE_ADDR) as usize, ring_pa);
            tcm.w32((mem + RING_BASE_ADDR + 4) as usize, 0);
            tcm.w16((mem + RING_MAX_ITEM) as usize, items);
            tcm.w16((mem + RING_LEN_ITEMS) as usize, size);
            mem += RING_MEM_SZ;
            let (w, r) = if k < 2 {
                h2d_w += 4;
                h2d_r += 4;
                (h2d_w - 4, h2d_r - 4)
            } else {
                d2h_w += 4;
                d2h_r += 4;
                (d2h_w - 4, d2h_r - 4)
            };
            rings.push(Ring {
                va,
                items,
                size,
                w_idx: w,
                r_idx: r,
                pos: 0,
                epoch: None,
            });
        }
        for (addr_off, len_off, len) in [
            (SHARED_SCRATCH_ADDR, SHARED_SCRATCH_LEN, SCRATCH_LEN),
            (SHARED_RINGUPD_ADDR, SHARED_RINGUPD_LEN, RINGUPD_LEN),
        ] {
            let (_, a) = dma.alloc(len, 8)?;
            tcm.w32((up.shared + addr_off) as usize, a);
            tcm.w32((up.shared + addr_off + 4) as usize, 0);
            tcm.w32((up.shared + len_off) as usize, len as u32);
        }
        let ioctl_buf = dma.alloc(IOCTL_MAX.next_multiple_of(64), 64)?;
        let mut ctl = Vec::new();
        for _ in 0..2 * CTL_BUFS {
            ctl.push(dma.alloc(CTL_BUF_LEN, 64)?);
        }
        let mut m = Msgbuf {
            _dma: dma,
            rings,
            pcie2,
            ioctl_buf,
            ctl,
            trans_id: 0,
            rx_dataoffset: t(up.shared + SHARED_RX_DATAOFFSET),
            events: VecDeque::new(),
            reply: None,
            console: up.console,
        };
        for id in 0..2 * CTL_BUFS {
            m.post_ctl(bp, tcm, id as u32)?;
        }
        Ok(m)
    }

    /// Room for one more item on an H2D ring: one slot always stays free.
    fn reserve(&self, tcm: &Mmio, ring: usize) -> Result<*mut u8, String> {
        let r = &self.rings[ring];
        let deadline = time::now_ns() + 100_000_000;
        loop {
            let read = tcm.r16(r.r_idx as usize);
            if (r.pos + 1) % r.items != read {
                let p = r.item(r.pos);
                unsafe { core::ptr::write_bytes(p, 0, r.size as usize) };
                return Ok(p);
            }
            if time::now_ns() > deadline {
                return Err(format!("ring {} full", ring));
            }
            time::sleep_us(100);
        }
    }

    /// Item written: publish the new write index and ring the doorbell.
    fn commit(&mut self, bp: &Backplane, tcm: &Mmio, ring: usize) {
        let r = &mut self.rings[ring];
        r.pos = (r.pos + 1) % r.items;
        dma_wmb();
        tcm.w16(r.w_idx as usize, r.pos);
        bp.w32(self.pcie2.base + PCIE2_H2D_MAILBOX_0, 1);
    }

    /// Hand control buffer `id` back to the firmware, for an ioctl
    /// response (ids 0..8) or an event (8..16).
    fn post_ctl(&mut self, bp: &Backplane, tcm: &Mmio, id: u32) -> Result<(), String> {
        let p = self.reserve(tcm, CONTROL_SUBMIT)?;
        let kind = if (id as usize) < CTL_BUFS {
            MSG_IOCTLRESP_BUF_POST
        } else {
            MSG_EVENT_BUF_POST
        };
        let (_, pa) = self.ctl[id as usize];
        unsafe { *p = kind };
        put32(p, 4, id);
        put16(p, 8, CTL_BUF_LEN as u16);
        put32(p, 16, pa);
        put32(p, 20, 0);
        self.commit(bp, tcm, CONTROL_SUBMIT);
        Ok(())
    }

    /// Next D2H item of `ring`, if the firmware wrote one; its sequence
    /// number is checked so a half-transferred item is never read.
    fn next(&mut self, tcm: &Mmio, ring: usize) -> Result<Option<Vec<u8>>, String> {
        let r = &mut self.rings[ring];
        let write = tcm.r16(r.w_idx as usize);
        if write == r.pos {
            return Ok(None);
        }
        // A dead or sleeping chip reads all ones: never chase that index.
        if write >= r.items {
            return Err(format!(
                "ring {} write index {:#x} of {} items",
                ring, write, r.items
            ));
        }
        let p = r.item(r.pos);
        if let Some(want) = r.epoch {
            let deadline = time::now_ns() + 1_000_000;
            while unsafe { core::ptr::read_volatile(p.add(3)) } as u32 != want
                && time::now_ns() < deadline
            {
                core::hint::spin_loop();
            }
        }
        let mut item = alloc::vec![0u8; r.size as usize];
        unsafe { core::ptr::copy_nonoverlapping(p, item.as_mut_ptr(), item.len()) };
        r.epoch = Some((item[3] as u32 + 1) % EPOCH_MODULO);
        r.pos = (r.pos + 1) % r.items;
        tcm.w16(r.r_idx as usize, r.pos);
        Ok(Some(item))
    }

    /// Contents of control buffer `id`, from where the firmware starts
    /// its data, then the buffer back to the firmware.
    fn take_ctl(
        &mut self,
        bp: &Backplane,
        tcm: &Mmio,
        id: u32,
        skip: usize,
        len: usize,
    ) -> Result<Vec<u8>, String> {
        let (va, _) = self.ctl[id as usize];
        let n = len.min(CTL_BUF_LEN.saturating_sub(skip));
        let mut out = alloc::vec![0u8; n];
        unsafe { core::ptr::copy_nonoverlapping(va.add(skip), out.as_mut_ptr(), n) };
        self.post_ctl(bp, tcm, id)?;
        Ok(out)
    }

    /// Drain the control completion ring: ioctl answers are kept for
    /// `ioctl`, event frames queued for `event`.
    pub fn poll(&mut self, bp: &Backplane, tcm: &Mmio) -> Result<(), String> {
        while let Some(item) = self.next(tcm, CONTROL_COMPLETE)? {
            let id = get32(item.as_ptr(), 4);
            match item[0] {
                MSG_IOCTL_CMPLT if (id as usize) < CTL_BUFS => {
                    let status = get16(item.as_ptr(), 8) as i16;
                    let len = get16(item.as_ptr(), 12) as usize;
                    let data = self.take_ctl(bp, tcm, id, 0, len)?;
                    self.reply = Some(Reply { status, data });
                }
                MSG_WL_EVENT if (CTL_BUFS..2 * CTL_BUFS).contains(&(id as usize)) => {
                    let len = get16(item.as_ptr(), 12) as usize;
                    let skip = self.rx_dataoffset as usize;
                    let frame = self.take_ctl(bp, tcm, id, skip, len)?;
                    if self.events.len() == EVENTS_MAX {
                        self.events.pop_front();
                    }
                    self.events.push_back(frame);
                }
                // Acks, and status messages this driver has no use for.
                _ => {}
            }
        }
        Ok(())
    }

    /// Oldest firmware event frame not read yet.
    pub fn event(&mut self) -> Option<Vec<u8>> {
        self.events.pop_front()
    }

    /// One ioctl (`brcmf_msgbuf_query_dcmd`): request, then the control
    /// completion ring polled until its answer; events are kept.
    pub fn ioctl(
        &mut self,
        bp: &Backplane,
        tcm: &Mmio,
        cmd: u32,
        data: &[u8],
        out_len: usize,
    ) -> Result<Reply, String> {
        let len = data.len().max(out_len).min(IOCTL_MAX);
        let (buf, buf_pa) = self.ioctl_buf;
        unsafe {
            core::ptr::write_bytes(buf, 0, len);
            core::ptr::copy_nonoverlapping(data.as_ptr(), buf, data.len().min(len));
        }
        self.trans_id = self.trans_id.wrapping_add(1);
        self.reply = None;
        let p = self.reserve(tcm, CONTROL_SUBMIT)?;
        unsafe { *p = MSG_IOCTLPTR_REQ };
        put32(p, 4, IOCTL_REQ_PKTID);
        put32(p, 8, cmd);
        put16(p, 12, self.trans_id);
        put16(p, 14, len as u16);
        put16(p, 16, len as u16);
        put32(p, 24, buf_pa);
        put32(p, 28, 0);
        self.commit(bp, tcm, CONTROL_SUBMIT);

        let deadline = time::now_ns() + IOCTL_TIMEOUT_MS * 1_000_000;
        loop {
            self.poll(bp, tcm)?;
            if let Some(r) = self.reply.take() {
                return Ok(r);
            }
            if time::now_ns() > deadline {
                return Err(format!(
                    "no answer to ioctl {} in {} ms",
                    cmd, IOCTL_TIMEOUT_MS
                ));
            }
            time::sleep_us(200);
        }
    }

    /// Firmware variable (`brcmf_fil_iovar_data_get`): name then NUL.
    pub fn get_var(
        &mut self,
        bp: &Backplane,
        tcm: &Mmio,
        name: &str,
        len: usize,
    ) -> Result<Vec<u8>, String> {
        let mut req = Vec::from(name.as_bytes());
        req.push(0);
        let r = self.ioctl(bp, tcm, WLC_GET_VAR, &req, len)?;
        if r.status != 0 {
            return Err(format!("{}: firmware error {}", name, r.status));
        }
        Ok(r.data)
    }

    /// Set a firmware variable (`brcmf_fil_iovar_data_set`).
    pub fn set_var(
        &mut self,
        bp: &Backplane,
        tcm: &Mmio,
        name: &str,
        value: &[u8],
    ) -> Result<(), String> {
        let mut req = Vec::from(name.as_bytes());
        req.push(0);
        req.extend_from_slice(value);
        let r = self.ioctl(bp, tcm, WLC_SET_VAR, &req, 0)?;
        if r.status != 0 {
            return Err(format!("{}: firmware error {}", name, r.status));
        }
        Ok(())
    }

    /// Integer command (`brcmf_fil_cmd_int_set`).
    pub fn cmd_int(
        &mut self,
        bp: &Backplane,
        tcm: &Mmio,
        cmd: u32,
        value: u32,
    ) -> Result<(), String> {
        let r = self.ioctl(bp, tcm, cmd, &value.to_le_bytes(), 0)?;
        if r.status != 0 {
            return Err(format!("command {}: firmware error {}", cmd, r.status));
        }
        Ok(())
    }
}
