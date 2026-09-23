//! Process: address space, capability table, threads.

use nxk_abi::err::{EINVAL, ENOMEM, ESRCH, ETIMEDOUT};
use nxk_abi::layout::{MAX_THREADS_PER_PROCESS, PAGE_SIZE, UTCB_BASE};
use nxk_abi::prot;

use crate::kern::bkl::{self, BklCell};
use crate::kern::objtab::Table;
use crate::kern::sched::Timeout;
use crate::kern::thread::{self, Queue, State, NIL};
use crate::kern::{cap, ipc, notify, percpu, sched, shm};
use crate::mm::{pmm, vm};
use crate::{kprintln, sysreg_write};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum PState {
    Free = 0,
    Alive,
    /// Exited; lingers while handles name it (exit code).
    Zombie,
}

pub struct Process {
    pub state: PState,
    pub gen: u16,
    pub free_link: u16,
    pub pid: u32,
    pub name: [u8; 16],
    pub asid: u16,
    pub l1: u64,
    /// TTBR0_EL1 value: table | ASID << 48.
    pub ttbr: u64,
    /// Capability table (physical address) and entry count.
    pub caps: u64,
    pub caps_len: u32,
    pub cap_hint: u32,
    pub nthreads: u16,
    /// Process thread list (chained via `Tcb::proc_next`).
    pub threads_head: u16,
    /// Shared-memory mappings (list of `shm::Mapping`).
    pub shm_head: u16,
    /// Debugger endpoint (`NIL`: none) and its generation.
    pub debug_ep: u16,
    pub debug_gen: u16,
    pub refs: u32,
    pub exit_code: i64,
    pub waiters: Queue,
    pub heap_next: u64,
    pub mmio_next: u64,
    /// Main stack top (randomized).
    pub stack_top: u64,
    /// Executable load base (fault reports).
    pub load_base: u64,
    /// Occupied UTCB slots (one bit per thread).
    pub utcb_map: [u64; MAX_THREADS_PER_PROCESS / 64],
    pub pages: u64,
    pub parent: u16,
    /// CPU time of all its threads (counter units).
    pub cpu_time: u64,
}

fn p_reset(p: &mut Process) {
    p.state = PState::Free;
    p.waiters = Queue::new();
    p.parent = NIL;
    p.threads_head = NIL;
    p.shm_head = NIL;
}

fn p_link(p: &mut Process) -> &mut u16 {
    &mut p.free_link
}

pub static PROCS: BklCell<Table<Process>> = BklCell::new(Table::new(p_reset, p_link));
static mut NEXT_PID: u32 = 1;

#[inline(always)]
pub fn get(i: u16) -> &'static mut Process {
    PROCS.get().get(i)
}

pub fn capacity() -> usize {
    PROCS.get().capacity()
}

pub fn init() {}

pub fn alive(i: u16, gen: u16) -> bool {
    (i as usize) < capacity() && get(i).gen == gen && get(i).state != PState::Free
}

pub fn incref(i: u16) {
    get(i).refs += 1;
}

pub fn decref(i: u16) {
    let p = get(i);
    p.refs -= 1;
    if p.refs == 0 && p.state == PState::Zombie {
        free_slot(i);
    }
}

fn free_slot(i: u16) {
    let p = get(i);
    p.state = PState::Free;
    p.gen = p.gen.wrapping_add(1);
    PROCS.get().free(i);
}

pub fn name(i: u16) -> &'static str {
    let n = &get(i).name;
    let len = n.iter().position(|&b| b == 0).unwrap_or(n.len());
    core::str::from_utf8(&n[..len]).unwrap_or("?")
}

/// Create an empty process (address space + capability table).
pub fn create(name: &[u8], parent: u16) -> Result<u16, i64> {
    let i = PROCS.get().alloc().ok_or(-ENOMEM)?;
    let l1 = match vm::create() {
        Some(l1) => l1,
        None => {
            PROCS.get().free(i);
            return Err(-ENOMEM);
        }
    };
    let (caps, caps_len) = match cap::alloc_table() {
        Some(c) => c,
        None => {
            pmm::free(l1, 0);
            PROCS.get().free(i);
            return Err(-ENOMEM);
        }
    };
    let p = get(i);
    let gen = p.gen;
    let asid = i + 1;
    // ASLR: heap, devices and stack shifted by a random page count (16 bits
    // each, up to 256 MiB).
    let r = crate::kern::random::next_u64();
    let shift = |k: u32| ((r >> (16 * k)) & 0xFFFF) * PAGE_SIZE;
    *p = Process {
        state: PState::Alive,
        gen,
        free_link: NIL,
        pid: unsafe {
            let v = NEXT_PID;
            NEXT_PID += 1;
            v
        },
        name: [0; 16],
        asid,
        l1,
        ttbr: l1 | ((asid as u64) << 48),
        caps,
        caps_len,
        cap_hint: 1,
        nthreads: 0,
        threads_head: NIL,
        shm_head: NIL,
        debug_ep: NIL,
        debug_gen: 0,
        refs: 0,
        exit_code: 0,
        waiters: Queue::new(),
        heap_next: nxk_abi::layout::HEAP_BASE + shift(0),
        mmio_next: nxk_abi::layout::MMIO_BASE + shift(1),
        stack_top: nxk_abi::layout::STACK_TOP - shift(2),
        load_base: 0,
        utcb_map: [0; MAX_THREADS_PER_PROCESS / 64],
        pages: 0,
        parent,
        cpu_time: 0,
    };
    let n = name.len().min(15);
    p.name[..n].copy_from_slice(&name[..n]);
    Ok(i)
}

/// Map fresh (zeroed) pages owned by the process: the range is taken
/// first, then filled letting other cores take the global lock every
/// `BREATHE_PAGES` (a page unmapped meanwhile is skipped). On failure
/// nothing stays mapped.
pub fn map_anon(i: u16, va: u64, len: u64, prot: u64) -> Result<(), i64> {
    reserve_anon(i, va, len, prot)?;
    let mut off = 0;
    while off < len {
        let r = if off > 0 && (off / PAGE_SIZE).is_multiple_of(bkl::BREATHE_PAGES) {
            bkl::breathe()
        } else {
            Ok(())
        };
        match r.and_then(|()| vm::populate(get(i).l1, va + off)) {
            Ok(true) => get(i).pages += 1,
            Ok(false) => {}
            Err(e) => {
                unmap(i, va, len);
                return Err(e);
            }
        }
        off += PAGE_SIZE;
    }
    Ok(())
}

/// Reserve anonymous pages allocated on first touch (like `mmap`),
/// letting other cores in every `BREATHE_PAGES`. On failure nothing
/// stays reserved.
pub fn reserve_anon(i: u16, va: u64, len: u64, prot: u64) -> Result<(), i64> {
    let flags = vm::user_flags(prot) | vm::SW_OWNED;
    let mut off = 0;
    while off < len {
        let r = if off > 0 && (off / PAGE_SIZE).is_multiple_of(bkl::BREATHE_PAGES) {
            bkl::breathe()
        } else {
            Ok(())
        };
        if let Err(e) = r.and_then(|()| vm::reserve(get(i).l1, va + off, flags)) {
            unmap(i, va, off);
            return Err(e);
        }
        off += PAGE_SIZE;
    }
    Ok(())
}

/// Populate a reserved page of the current process (kernel access to its own
/// memory).
pub fn populate_current(va: u64) -> bool {
    let cur = percpu::this().current;
    cur != NIL && populate(thread::get(cur).proc_idx, va)
}

/// Translation fault: populate the page if it was reserved.
pub fn populate(i: u16, va: u64) -> bool {
    let p = get(i);
    // Already populated: two threads touched the same fresh page on two cores,
    // and the other won the race. The fault is resolved.
    if vm::lookup(p.l1, va & !(PAGE_SIZE - 1)) & 1 != 0 {
        return true;
    }
    match vm::populate(p.l1, va & !(PAGE_SIZE - 1)) {
        Ok(true) => {
            p.pages += 1;
            true
        }
        _ => false,
    }
}

/// Remove and free pages (only owned pages return to the allocator; device
/// registers and shared memory stay). By batches of `BREATHE_PAGES`:
/// entries cleared, one TLB flush, then what they held is released and
/// other cores may take the global lock.
pub fn unmap(i: u16, va: u64, len: u64) {
    const BATCH: usize = bkl::BREATHE_PAGES as usize;
    let pages = len / PAGE_SIZE;
    let mut k = 0;
    while k < pages {
        // Killed meanwhile: the rest goes with the address space.
        if k > 0 && bkl::breathe().is_err() {
            return;
        }
        let p = get(i);
        let n = (pages - k).min(BATCH as u64);
        let mut cleared = [(0u64, 0u64); BATCH];
        let mut nc = 0;
        let mut live = 0;
        for j in 0..n {
            let a = va + (k + j) * PAGE_SIZE;
            if let Some(old) = vm::clear(p.l1, a) {
                live += vm::is_valid(old) as usize;
                cleared[nc] = (a, old);
                nc += 1;
            }
        }
        // No core may still reach a page when it is freed.
        if live > 8 {
            vm::flush_asid(p.asid);
        } else {
            for &(a, old) in &cleared[..nc] {
                if vm::is_valid(old) {
                    vm::flush_page(p.asid, a);
                }
            }
        }
        for &(a, old) in &cleared[..nc] {
            if let Some(pa) = vm::owned_pa(old) {
                pmm::free(pa, 0);
                get(i).pages -= 1;
            } else if old & vm::SW_SHM != 0 {
                shm::page_unmapped(i, a);
            }
        }
        k += n;
    }
}

/// Create a user thread in the process. It starts at the scheduler's next
/// pass on its core.
pub fn create_thread(
    i: u16,
    entry: u64,
    sp: u64,
    args: [u64; 3],
    prio: u8,
    cpu: Option<usize>,
) -> Result<u16, i64> {
    let p = get(i);
    if p.state != PState::Alive {
        return Err(-ESRCH);
    }
    let slot = p
        .utcb_map
        .iter()
        .enumerate()
        .find(|(_, &w)| w != u64::MAX)
        .map(|(k, &w)| k * 64 + (!w).trailing_zeros() as usize)
        .ok_or(-ENOMEM)?;
    let ti = thread::alloc().ok_or(-ENOMEM)?;

    // UTCB: one RW page for the thread, read by the kernel via its physical
    // address.
    let utcb_va = UTCB_BASE + slot as u64 * PAGE_SIZE;
    let utcb_pa = match pmm::alloc_zeroed(0) {
        Some(pa) => pa,
        None => {
            thread::free(ti);
            return Err(-ENOMEM);
        }
    };
    if let Err(e) = vm::map(
        p.l1,
        utcb_va,
        utcb_pa,
        vm::user_flags(prot::READ | prot::WRITE) | vm::SW_OWNED,
    ) {
        pmm::free(utcb_pa, 0);
        thread::free(ti);
        return Err(e);
    }
    vm::publish();
    p.pages += 1;
    p.utcb_map[slot / 64] |= 1 << (slot % 64);
    p.nthreads += 1;

    let t = thread::get(ti);
    t.proc_idx = i;
    t.proc_next = p.threads_head;
    p.threads_head = ti;
    t.utcb_slot = slot as u16;
    t.utcb_va = utcb_va;
    t.utcb_pa = utcb_pa;
    t.prio = if prio == 0 {
        nxk_abi::thread::DEFAULT_PRIO
    } else {
        prio.min(nxk_abi::thread::MAX_PRIO)
    };
    let pinned = cpu.filter(|&c| c < percpu::NR_CPUS && percpu::get(c).online);
    t.affinity = pinned.map(|c| c as u8).unwrap_or(thread::ANY_CPU);
    t.home = pinned.unwrap_or_else(sched::least_loaded) as u8;
    t.frame.elr = entry;
    t.frame.sp = sp & !0xF;
    t.frame.spsr = 0;
    t.frame.x[0] = args[0];
    t.frame.x[1] = args[1];
    t.frame.x[2] = args[2];
    t.fp.fpcr = 0;
    sched::make_ready(ti);
    Ok(ti)
}

/// Kill a process: all its threads, then its resources.
pub fn kill(i: u16, code: i64) {
    let p = get(i);
    if p.state != PState::Alive {
        return;
    }
    p.exit_code = code;
    let this_cpu = percpu::this().id;
    let mut ti = p.threads_head;
    while ti != NIL {
        let t = thread::get(ti);
        let next = t.proc_next;
        if t.state == State::Zombie {
            ti = next;
            continue;
        }
        let running_elsewhere =
            (0..percpu::NR_CPUS).any(|c| c != this_cpu && percpu::get(c).current == ti);
        sched::detach(ti);
        if running_elsewhere {
            // The core running it frees it at its next kernel entry.
            t.state = State::Zombie;
            sched::kick(t.home as usize);
        } else if percpu::this().current == ti {
            t.state = State::Zombie;
        } else {
            reap(ti);
        }
        ti = next;
    }
}

/// Thread of process `i` with ID `tid`.
pub fn find_thread(i: u16, tid: u32) -> Option<u16> {
    let mut ti = get(i).threads_head;
    while ti != NIL {
        let t = thread::get(ti);
        if t.tid == tid && t.state != State::Zombie {
            return Some(ti);
        }
        ti = t.proc_next;
    }
    None
}

/// Free a dead thread. If it was the last, the process goes away.
pub fn reap(ti: u16) {
    let t = thread::get(ti);
    let i = t.proc_idx;
    if t.timed {
        sched::disarm(ti);
    }
    sched::forget_thread(ti);
    let t = thread::get(ti);
    if t.bound_notif != NIL {
        notify::unbind(t.bound_notif);
    }
    // A server owing this thread a reply must not later answer its slot
    // successor.
    if t.reply_holder != NIL && thread::get(t.reply_holder).reply_to == ti {
        thread::get(t.reply_holder).reply_to = NIL;
    }
    let slot = t.utcb_slot;
    unlink_thread(i, ti);
    thread::free(ti);

    let p = get(i);
    p.nthreads -= 1;
    if p.nthreads == 0 {
        finalize(i);
    } else if slot != NIL {
        // Slot reused by a future thread: its page must go.
        unmap(i, UTCB_BASE + slot as u64 * PAGE_SIZE, PAGE_SIZE);
        p.utcb_map[slot as usize / 64] &= !(1 << (slot % 64));
    }
}

/// Drop a process whose launch failed before any thread.
pub fn abort_unstarted(i: u16) {
    get(i).exit_code = 0;
    finalize(i);
}

fn unlink_thread(i: u16, ti: u16) {
    let p = get(i);
    if p.threads_head == ti {
        p.threads_head = thread::get(ti).proc_next;
        return;
    }
    let mut cur = p.threads_head;
    while cur != NIL {
        let next = thread::get(cur).proc_next;
        if next == ti {
            thread::get(cur).proc_next = thread::get(ti).proc_next;
            return;
        }
        cur = next;
    }
}

/// Last step of process exit, once all threads are freed.
fn finalize(i: u16) {
    let p = get(i);
    // No core may keep a pointer to these page tables.
    let cpu = percpu::this();
    if cpu.cur_ttbr == p.ttbr {
        let k = crate::arch::mmu::kernel_ttbr();
        sysreg_write!("ttbr0_el1", k);
        crate::arch::isb();
        cpu.cur_ttbr = k;
    }
    ipc::abort_process(i);
    crate::kern::debug::release(i);
    cap::close_all(i);
    cap::free_table(p.caps, p.caps_len);
    vm::destroy(p.l1, p.asid);
    shm::drop_mappings(i);
    p.pages = 0;

    let code = p.exit_code;
    if code != 0 {
        kprintln!(
            "[nxk] process {} '{}' exited (code {})",
            p.pid,
            name(i),
            code
        );
    }
    // Wake those waiting for this exit.
    while let Some(w) = p.waiters.pop() {
        sched::wake(w, code as u64);
    }
    p.state = PState::Zombie;
    if p.refs == 0 {
        free_slot(i);
    }
}

/// `process_wait`: immediate exit code, or park until exit.
pub fn wait(ti: u16, target: u16, tmo: Timeout) -> Result<Option<u64>, i64> {
    let p = get(target);
    match p.state {
        PState::Zombie => Ok(Some(p.exit_code as u64)),
        PState::Alive => {
            if tmo == Timeout::Now {
                return Err(-ETIMEDOUT);
            }
            let t = thread::get(ti);
            t.state = State::BlockedWait;
            t.blocked_on = target;
            p.waiters.push(ti);
            tmo.arm(ti);
            Ok(None)
        }
        PState::Free => Err(-EINVAL),
    }
}
