//! Broadcom msgbuf protocol over PCIe (Linux `brcmfmac`: `pcie.c`,
//! `commonring.c`, `msgbuf.c`): rings in host memory, their indices in
//! the chip's RAM, doorbells through the PCIe2 core's mailbox. Control
//! (ioctls, events) and data: received frames, and one TX flow ring.

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
const SHARED_MAX_RXBUFPOST: u32 = 34;
const SHARED_RX_DATAOFFSET: u32 = 36;
const SHARED_DTOH_MB_DATA: u32 = 44;
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
const RXPOST_SUBMIT: usize = 1;
const CONTROL_COMPLETE: usize = 2;
const TX_COMPLETE: usize = 3;
const RX_COMPLETE: usize = 4;
const RINGS: [(u16, u16); 5] = [(64, 40), (512, 32), (64, 24), (1024, 16), (512, 32)];
/// The TX flow ring, after the common ones: its indices follow theirs
/// in TCM, its id follows the two H2D ones (`_FLOWRING_IDSTART`).
const FLOW: usize = 5;
const FLOW_RING_ID: u16 = 2;
const FLOW_ITEMS: u16 = 512;
const FLOW_ITEM_SIZE: u16 = 48;
const FLOW_TIMEOUT_MS: u64 = 1000;

// PCIe2 core mailbox (core rev < 64).
const PCIE2_MAILBOXINT: u32 = 0x48;
const PCIE2_MAILBOXMASK: u32 = 0x4C;
const PCIE2_H2D_MAILBOX_0: u32 = 0x140;
/// Interrupt causes: function 0 mailbox data, D2H doorbells (the
/// firmware wrote completions).
const MB_INT_FN0: u32 = 0x0300;
const MB_INT_D2H_DB: u32 = 0x00FF_0000;
// Mailbox data from the firmware (`BRCMF_D2H_DEV_*`).
pub const D2H_DS_ENTER_REQ: u32 = 0x2;
pub const D2H_FWHALT: u32 = 0x1000_0000;

// Message types.
const MSG_FLOW_RING_CREATE: u8 = 0x3;
const MSG_FLOW_RING_CREATE_CMPLT: u8 = 0x4;
const MSG_IOCTLPTR_REQ: u8 = 0x9;
const MSG_IOCTLPTR_REQ_ACK: u8 = 0xA;
const MSG_IOCTLRESP_BUF_POST: u8 = 0xB;
const MSG_IOCTL_CMPLT: u8 = 0xC;
const MSG_EVENT_BUF_POST: u8 = 0xD;
const MSG_WL_EVENT: u8 = 0xE;
const MSG_TX_POST: u8 = 0xF;
const MSG_TX_STATUS: u8 = 0x10;
const MSG_RXBUF_POST: u8 = 0x11;
const MSG_RX_CMPLT: u8 = 0x12;
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
/// Data buffers (`BRCMF_MSGBUF_MAX_PKT_SIZE`): frames received, frames
/// to send. Enough for key handshakes and light traffic.
const PKT_LEN: usize = 2048;
const RX_BUFS: usize = 64;
const TX_BUFS: usize = 32;
const ETH_HLEN: usize = 14;
/// TX post flags: an Ethernet frame, priority 0 (best effort).
const PKT_FRAME_802_3: u8 = 0x01;
/// Frames kept for a reader; older ones are dropped past this.
const FRAMES_MAX: usize = 64;
/// Host memory for rings and buffers.
const DMA_LEN: usize = 512 * 1024;

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

/// State of the TX flow ring, created on the first frame sent.
#[derive(Clone, Copy, PartialEq)]
enum Flow {
    Closed,
    Pending,
    Open,
    Refused(u16),
}

/// A frame buffer the firmware reads for a send.
struct TxBuf {
    va: *mut u8,
    pa: u32,
    busy: bool,
    /// Firmware status of its last send.
    status: u16,
}

pub struct Msgbuf {
    _dma: Dma,
    /// The five common rings, then the TX flow ring.
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
    /// Receive buffers by request id.
    rx: Vec<(*mut u8, u32)>,
    /// Frames received (Ethernet), oldest first.
    frames: VecDeque<Vec<u8>>,
    tx: Vec<TxBuf>,
    flow: Flow,
    flow_pa: u32,
    /// Frames received and sent (posted, then completed), and the
    /// Ethernet header of the last one received: for `wifi status`.
    count: [u32; 3],
    last_rx: [u8; 14],
    /// D2H items announced but not landed yet when first looked at.
    late: u32,
    /// Other control completions: count, and the last one.
    other: (u32, [u8; 24]),
    reply: Option<Reply>,
    /// The firmware console, for `console_tail`.
    pub console: u32,
    /// Mailbox data word from the firmware, in TCM.
    dtoh_mb: u32,
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
        let shm = SharedMemory::new(DMA_LEN, CONTIGUOUS | UNCACHED | DMA32)
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
            });
        }
        // Flow ring: host memory now, created in the firmware on demand.
        let (va, flow_pa) = dma.alloc(FLOW_ITEMS as usize * FLOW_ITEM_SIZE as usize, 8)?;
        rings.push(Ring {
            va,
            items: FLOW_ITEMS,
            size: FLOW_ITEM_SIZE,
            w_idx: h2d_w,
            r_idx: h2d_r,
            pos: 0,
        });
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
        // As many receive buffers as the firmware takes, up to ours.
        let max_rx = match tcm.r16((up.shared + SHARED_MAX_RXBUFPOST) as usize) {
            0 => RX_BUFS,
            n => RX_BUFS.min(n as usize),
        };
        let mut rx = Vec::new();
        for _ in 0..max_rx {
            rx.push(dma.alloc(PKT_LEN, 64)?);
        }
        let mut tx = Vec::new();
        for _ in 0..TX_BUFS {
            let (va, pa) = dma.alloc(PKT_LEN, 64)?;
            tx.push(TxBuf {
                va,
                pa,
                busy: false,
                status: 0,
            });
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
            rx,
            frames: VecDeque::new(),
            tx,
            flow: Flow::Closed,
            flow_pa,
            count: [0; 3],
            last_rx: [0; 14],
            late: 0,
            other: (0, [0; 24]),
            reply: None,
            console: up.console,
            dtoh_mb: t(up.shared + SHARED_DTOH_MB_DATA),
        };
        for id in 0..2 * CTL_BUFS {
            m.post_ctl(bp, tcm, id as u32)?;
        }
        for id in 0..m.rx.len() {
            m.post_rx(bp, tcm, id as u32)?;
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

    /// Chip interrupts on (`brcmf_pcie_intr_enable`): completions and
    /// mailbox data raise INTx.
    pub fn irq_enable(&self, bp: &Backplane) {
        bp.w32(
            self.pcie2.base + PCIE2_MAILBOXMASK,
            MB_INT_D2H_DB | MB_INT_FN0,
        );
    }

    /// What an interrupt was for (`brcmf_pcie_isr_thread`): its causes,
    /// cleared so INTx drops before the line is re-armed, and the mailbox
    /// data that came with it, taken.
    pub fn irq_take(&self, bp: &Backplane, tcm: &Mmio) -> (u32, u32) {
        let status = bp.r32(self.pcie2.base + PCIE2_MAILBOXINT);
        if status != 0 {
            bp.w32(self.pcie2.base + PCIE2_MAILBOXINT, status);
        }
        let mut data = 0;
        if status & MB_INT_FN0 != 0 {
            data = tcm.r32(self.dtoh_mb as usize);
            if data != 0 {
                tcm.w32(self.dtoh_mb as usize, 0);
            }
        }
        (status, data)
    }

    /// The chip still answers: its mailbox register does not read all ones.
    pub fn alive(&self, bp: &Backplane) -> bool {
        bp.r32(self.pcie2.base + PCIE2_MAILBOXINT) != u32::MAX
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

    /// Hand receive buffer `id` to the firmware
    /// (`brcmf_msgbuf_rxbuf_data_post`).
    fn post_rx(&mut self, bp: &Backplane, tcm: &Mmio, id: u32) -> Result<(), String> {
        let p = self.reserve(tcm, RXPOST_SUBMIT)?;
        let (_, pa) = self.rx[id as usize];
        unsafe { *p = MSG_RXBUF_POST };
        put32(p, 4, id);
        put16(p, 10, PKT_LEN as u16);
        put32(p, 24, pa);
        put32(p, 28, 0);
        self.commit(bp, tcm, RXPOST_SUBMIT);
        Ok(())
    }

    /// Next D2H item of `ring`, if the firmware wrote one and it landed.
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
        // The index can be seen before the item lands, right after the
        // interrupt. Items are cleared once read and no message has type
        // 0 (this firmware leaves the sequence number of control
        // completions at 0): one still reading 0 waits for the next poll.
        let p = r.item(r.pos);
        let deadline = time::now_ns() + 1_000_000;
        while unsafe { core::ptr::read_volatile(p) } == 0 {
            if time::now_ns() > deadline {
                self.late += 1;
                return Ok(None);
            }
            core::hint::spin_loop();
        }
        let mut item = alloc::vec![0u8; r.size as usize];
        unsafe {
            core::ptr::copy_nonoverlapping(p, item.as_mut_ptr(), item.len());
            core::ptr::write_bytes(p, 0, item.len());
        }
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

    /// Drain the completion rings: ioctl answers are kept for `ioctl`,
    /// event frames queued for `event`, received frames for `frame`,
    /// finished sends noted for `sent`.
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
                MSG_FLOW_RING_CREATE_CMPLT => {
                    self.flow = match get16(item.as_ptr(), 8) {
                        0 => Flow::Open,
                        status => Flow::Refused(status),
                    };
                }
                MSG_IOCTLPTR_REQ_ACK => {}
                // Status messages: the firmware reports ring and general
                // errors there, kept for `wifi status`.
                t => {
                    let _ = t;
                    self.other.0 += 1;
                    self.other.1.copy_from_slice(&item[..24]);
                }
            }
        }
        // `brcmf_msgbuf_process_rx_complete`: the frame starts at its
        // offset, or at the firmware's default one.
        while let Some(item) = self.next(tcm, RX_COMPLETE)? {
            let id = get32(item.as_ptr(), 4) as usize;
            if item[0] != MSG_RX_CMPLT || id >= self.rx.len() {
                continue;
            }
            let len = get16(item.as_ptr(), 14) as usize;
            let off = match get16(item.as_ptr(), 16) {
                0 => self.rx_dataoffset as usize,
                o => o as usize,
            };
            let (va, _) = self.rx[id];
            let n = len.min(PKT_LEN.saturating_sub(off));
            let mut frame = alloc::vec![0u8; n];
            unsafe { core::ptr::copy_nonoverlapping(va.add(off), frame.as_mut_ptr(), n) };
            self.post_rx(bp, tcm, id as u32)?;
            self.count[0] += 1;
            let h = n.min(self.last_rx.len());
            self.last_rx[..h].copy_from_slice(&frame[..h]);
            if self.frames.len() == FRAMES_MAX {
                self.frames.pop_front();
            }
            self.frames.push_back(frame);
        }
        while let Some(item) = self.next(tcm, TX_COMPLETE)? {
            let id = get32(item.as_ptr(), 4) as usize;
            if item[0] != MSG_TX_STATUS {
                continue;
            }
            if let Some(b) = id.checked_sub(1).and_then(|k| self.tx.get_mut(k)) {
                b.busy = false;
                b.status = get16(item.as_ptr(), 8);
                self.count[2] += 1;
            }
        }
        Ok(())
    }

    /// The data path at a glance: frame counts, the last header seen,
    /// then the write/read indices of the RX post, RX and TX completion
    /// and flow rings.
    pub fn stats(&self, tcm: &Mmio) -> String {
        let idx = |k: usize| {
            let r = &self.rings[k];
            format!(
                "{}/{}",
                tcm.r16(r.w_idx as usize),
                tcm.r16(r.r_idx as usize)
            )
        };
        format!(
            "rx {} (last {:02x?}), tx {} sent {}, flow {}, late items {}, other control messages {} (last {:02x?}); rings rxpost {} rx {} txdone {} flow {}; next unread rx {:02x?} txdone {:02x?}",
            self.count[0],
            self.last_rx,
            self.count[1],
            self.count[2],
            match self.flow {
                Flow::Closed => "closed",
                Flow::Pending => "pending",
                Flow::Open => "open",
                Flow::Refused(_) => "refused",
            },
            self.late,
            self.other.0,
            self.other.1,
            idx(RXPOST_SUBMIT),
            idx(RX_COMPLETE),
            idx(TX_COMPLETE),
            idx(FLOW),
            self.peek(RX_COMPLETE),
            self.peek(TX_COMPLETE)
        )
    }

    /// Raw bytes of the next unread item of a D2H ring, and of the one
    /// after: what a stalled ring is stuck on.
    fn peek(&self, ring: usize) -> [Vec<u8>; 2] {
        let r = &self.rings[ring];
        let read = |k: u16| {
            let p = r.item(k % r.items);
            let mut v = alloc::vec![0u8; r.size as usize];
            unsafe { core::ptr::copy_nonoverlapping(p, v.as_mut_ptr(), v.len()) };
            v
        };
        [read(r.pos), read(r.pos + 1)]
    }

    /// Oldest frame received and not read yet.
    pub fn frame(&mut self) -> Option<Vec<u8>> {
        self.frames.pop_front()
    }

    /// Send an Ethernet frame (`brcmf_msgbuf_txflow`): its header in the
    /// ring item, its payload read from our buffer. Returns the request
    /// id `sent` answers for.
    pub fn send(&mut self, bp: &Backplane, tcm: &Mmio, frame: &[u8]) -> Result<u32, String> {
        if frame.len() < ETH_HLEN || frame.len() > PKT_LEN {
            return Err(format!("frame of {} bytes", frame.len()));
        }
        self.open_flow(bp, tcm, frame)?;
        self.poll(bp, tcm)?;
        let slot = self
            .tx
            .iter()
            .position(|b| !b.busy)
            .ok_or_else(|| String::from("every TX buffer in flight"))?;
        let p = self.reserve(tcm, FLOW)?;
        let b = &mut self.tx[slot];
        unsafe { core::ptr::copy_nonoverlapping(frame.as_ptr(), b.va, frame.len()) };
        b.busy = true;
        let pa = b.pa;
        let id = slot as u32 + 1;
        unsafe {
            *p = MSG_TX_POST;
            core::ptr::copy_nonoverlapping(frame.as_ptr(), p.add(8), ETH_HLEN);
            *p.add(22) = PKT_FRAME_802_3;
            *p.add(23) = 1; // one segment
        }
        put32(p, 4, id);
        put32(p, 32, pa + ETH_HLEN as u32);
        put32(p, 36, 0);
        put16(p, 42, (frame.len() - ETH_HLEN) as u16);
        self.commit(bp, tcm, FLOW);
        self.count[1] += 1;
        Ok(id)
    }

    /// A TX buffer is free for `send`.
    pub fn tx_free(&self) -> bool {
        self.tx.iter().any(|b| !b.busy)
    }

    /// Firmware status of send `id`, once it finished.
    pub fn sent(&self, id: u32) -> Option<u16> {
        let b = self.tx.get((id as usize).checked_sub(1)?)?;
        (!b.busy).then_some(b.status)
    }

    /// The TX flow ring, created in the firmware for the first frame
    /// (`brcmf_msgbuf_flowring_create_worker`); a station keeps one.
    fn open_flow(&mut self, bp: &Backplane, tcm: &Mmio, frame: &[u8]) -> Result<(), String> {
        if self.flow == Flow::Open {
            return Ok(());
        }
        let p = self.reserve(tcm, CONTROL_SUBMIT)?;
        unsafe {
            *p = MSG_FLOW_RING_CREATE;
            // Destination, then source: the frame's own addresses.
            core::ptr::copy_nonoverlapping(frame.as_ptr(), p.add(8), 12);
        }
        put16(p, 22, FLOW_RING_ID);
        put16(p, 28, FLOW_ITEMS);
        put16(p, 30, FLOW_ITEM_SIZE);
        put32(p, 32, self.flow_pa);
        put32(p, 36, 0);
        self.flow = Flow::Pending;
        self.commit(bp, tcm, CONTROL_SUBMIT);
        let deadline = time::now_ns() + FLOW_TIMEOUT_MS * 1_000_000;
        while self.flow == Flow::Pending && time::now_ns() < deadline {
            time::sleep_us(200);
            self.poll(bp, tcm)?;
        }
        match self.flow {
            Flow::Open => Ok(()),
            f => {
                self.flow = Flow::Closed;
                Err(match f {
                    Flow::Refused(status) => format!("TX flow ring refused (status {})", status),
                    _ => format!("TX flow ring not created in {} ms", FLOW_TIMEOUT_MS),
                })
            }
        }
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
