//! Thread TCBs: no per-thread stack; asm lands directly in Frame.
//! Reply tokens expire with the call; FP state is lazy (see `arch::fpu`).

use crate::kern::bkl::BklCell;
use crate::kern::objtab::Table;

pub const NIL: u16 = u16::MAX;
pub const NR_PRIO: usize = 32;
/// Free affinity: the kernel picks (and may change) the core.
pub const ANY_CPU: u8 = 0xFF;

/// User registers. Frozen layout: asm writes these offsets.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct Frame {
    pub x: [u64; 31], // @0
    pub sp: u64,      // @248 SP_EL0
    pub elr: u64,     // @256
    pub spsr: u64,    // @264
    pub tpidr: u64,   // @272 TPIDR_EL0 (TLS utilisateur)
    pub _pad: u64,    // @280
}

pub const FRAME_SIZE: usize = 288;
const _: () = assert!(core::mem::size_of::<Frame>() == FRAME_SIZE);

/// FP/SIMD state, saved lazily (see `arch::fpu`).
#[repr(C, align(16))]
#[derive(Clone, Copy)]
pub struct FpState {
    pub q: [u128; 32],
    pub fpcr: u64,
    pub fpsr: u64,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum State {
    Free = 0,
    Ready,
    Running,
    /// Sender queued on a rendezvous point.
    BlockedSend,
    /// Receiver waiting on a rendezvous point.
    BlockedRecv,
    /// Client after `call`, waiting for the reply.
    BlockedReply,
    BlockedNotify,
    /// Waits for a process to exit.
    BlockedWait,
    /// Waits on a futex (key: physical address).
    BlockedFutex,
    Sleeping,
    /// Stopped (debugging), waiting for a debugger.
    Stopped,
    /// Dead but running on another core: that core frees it.
    Zombie,
}

#[repr(C, align(64))]
pub struct Tcb {
    pub frame: Frame,
    pub fp: FpState,
    pub state: State,
    pub prio: u8,
    pub home: u8,
    /// In a run queue.
    pub queued: bool,
    /// In its core's deadline queue.
    pub timed: bool,
    /// Pinned core, or `ANY_CPU`.
    pub affinity: u8,
    pub gen: u16,
    /// Intrusive link: a thread is never in two queues at once.
    pub next: u16,
    pub proc_idx: u16,
    pub utcb_slot: u16,
    /// Client to reply to (server).
    pub reply_to: u16,
    /// Server process awaited for a reply (client).
    pub reply_from: u16,
    /// Object the thread parks on (endpoint, notification, process).
    pub blocked_on: u16,
    pub bound_notif: u16,
    /// Next thread of the same process.
    pub proc_next: u16,
    /// Server holding our reply (call in flight).
    pub reply_holder: u16,
    /// Core deadline queue (doubly linked, sorted by `wake_at`).
    pub tq_next: u16,
    pub tq_prev: u16,
    /// Pending migrations on the old core.
    pub mig_next: u16,
    pub send_is_call: bool,
    /// In a core's migration queue.
    pub mig_queued: bool,
    /// Ongoing debugger event: the debugger message comes from the
    /// `fault_*` fields and its reply leaves registers alone.
    pub in_fault: bool,
    pub fault_kind: u8,
    /// Debugger-requested stop, taken at the next kernel exit.
    pub stop_request: bool,
    /// Run one instruction, then a STEP event.
    pub single_step: bool,
    pub tid: u32,
    /// Call number: a reply token is only good for the current call.
    pub call_seq: u32,
    pub send_badge: u64,
    pub utcb_va: u64,
    pub utcb_pa: u64,
    /// Absolute deadline (counter) of a sleep or bounded wait.
    pub wake_at: u64,
    pub futex_key: u64,
    /// CPU time used (counter units).
    pub cpu_time: u64,
    pub fault_esr: u64,
    pub fault_far: u64,
}

impl Tcb {
    fn reset(&mut self, gen: u16) {
        // All zero (registers, FP state), then the non-zero fields.
        unsafe { core::ptr::write_bytes(self as *mut Tcb, 0, 1) };
        self.gen = gen;
        self.next = NIL;
        self.proc_idx = NIL;
        self.utcb_slot = NIL;
        self.reply_to = NIL;
        self.reply_from = NIL;
        self.blocked_on = NIL;
        self.bound_notif = NIL;
        self.proc_next = NIL;
        self.reply_holder = NIL;
        self.tq_next = NIL;
        self.tq_prev = NIL;
        self.mig_next = NIL;
        self.affinity = ANY_CPU;
        self.state = State::Free;
    }
}

fn tcb_reset(t: &mut Tcb) {
    t.reset(0);
}

fn tcb_link(t: &mut Tcb) -> &mut u16 {
    &mut t.next
}

pub static TCBS: BklCell<Table<Tcb>> = BklCell::new(Table::new(tcb_reset, tcb_link));
static mut NEXT_TID: u32 = 1;

#[inline(always)]
pub fn get(i: u16) -> &'static mut Tcb {
    TCBS.get().get(i)
}

/// Iteration bound over all created TCBs.
#[inline]
pub fn capacity() -> usize {
    TCBS.get().capacity()
}

pub fn init() {}

/// Take a free TCB, zeroed.
pub fn alloc() -> Option<u16> {
    let i = TCBS.get().alloc()?;
    let t = get(i);
    let gen = t.gen.wrapping_add(1);
    t.reset(gen);
    unsafe {
        t.tid = NEXT_TID;
        NEXT_TID += 1;
    }
    Some(i)
}

pub fn free(i: u16) {
    let t = get(i);
    let gen = t.gen;
    t.reset(gen);
    TCBS.get().free(i);
}

/// Reply token for one call; expires with the call so late replies miss.
#[inline]
pub fn token(i: u16) -> u64 {
    let t = get(i);
    ((t.call_seq as u64) << 32) | ((i as u64) << 16) | t.gen as u64
}

#[inline]
pub fn from_token(tok: u64) -> Option<u16> {
    let i = ((tok >> 16) & 0xFFFF) as usize;
    if i >= capacity() {
        return None;
    }
    let t = get(i as u16);
    (t.gen == tok as u16 && t.call_seq == (tok >> 32) as u32 && t.state != State::Free)
        .then_some(i as u16)
}

/// Intrusive FIFO of threads.
#[derive(Clone, Copy)]
pub struct Queue {
    pub head: u16,
    pub tail: u16,
}

impl Queue {
    pub const fn new() -> Self {
        Self {
            head: NIL,
            tail: NIL,
        }
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.head == NIL
    }

    #[inline]
    pub fn push(&mut self, i: u16) {
        get(i).next = NIL;
        if self.tail == NIL {
            self.head = i;
        } else {
            get(self.tail).next = i;
        }
        self.tail = i;
    }

    #[inline]
    pub fn pop(&mut self) -> Option<u16> {
        let i = self.head;
        if i == NIL {
            return None;
        }
        self.head = get(i).next;
        if self.head == NIL {
            self.tail = NIL;
        }
        get(i).next = NIL;
        Some(i)
    }

    /// Remove the first element matching `pred`.
    pub fn take(&mut self, mut pred: impl FnMut(u16) -> bool) -> Option<u16> {
        let mut prev = NIL;
        let mut cur = self.head;
        while cur != NIL {
            let next = get(cur).next;
            if pred(cur) {
                if prev == NIL {
                    self.head = next;
                } else {
                    get(prev).next = next;
                }
                if self.tail == cur {
                    self.tail = prev;
                }
                get(cur).next = NIL;
                return Some(cur);
            }
            prev = cur;
            cur = next;
        }
        None
    }

    pub fn remove(&mut self, i: u16) -> bool {
        self.take(|x| x == i).is_some()
    }
}
