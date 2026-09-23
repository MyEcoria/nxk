//! Syscalls: Linux arm64 numbers in x8, `-errno` results in x0.
//! Parking calls complete later via the waker's Frame write.

use nxk_abi::err::*;
use nxk_abi::info::{self, CpuInfo, MemInfo, ProcInfo};
use nxk_abi::layout::*;
use nxk_abi::spawn::{HandleGrant, SpawnArgs};
use nxk_abi::{clock, prot, rights, sys};

use crate::arch::timer;
use crate::kern::cap::{self, Cap, Kind};
use crate::kern::elf::{self, UserSource};
use crate::kern::exec;
use crate::kern::process::{self, PState};
use crate::kern::sched::Timeout;
use crate::kern::thread::{self, State};
use crate::kern::{debug, futex, ipc, irq, notify, percpu, sched, shm};
use crate::mm::{pmm, uaccess, vm};

/// `Ok(Some(v))`: value to return; `Ok(None)`: Frame already filled or thread
/// parked; `Err(e)`: error to return.
type SysResult = Result<Option<u64>, i64>;

#[inline]
fn val(v: u64) -> SysResult {
    Ok(Some(v))
}

/// Kernel-reserved device registers (GIC): never mappable.
const KERNEL_MMIO: core::ops::Range<u64> = 0x5004_0000..0x5006_0000;

pub fn dispatch(ti: u16) {
    let t = thread::get(ti);
    let nr = t.frame.x[8];
    let a = [
        t.frame.x[0],
        t.frame.x[1],
        t.frame.x[2],
        t.frame.x[3],
        t.frame.x[4],
        t.frame.x[5],
    ];
    let pi = t.proc_idx;
    percpu::this().stats.syscalls += 1;

    let r = handle(ti, pi, nr, a);
    let t = thread::get(ti);
    match r {
        Ok(Some(v)) => t.frame.x[0] = v,
        Ok(None) => {}
        Err(e) => t.frame.x[0] = e as u64,
    }
}

fn handle(ti: u16, pi: u16, nr: u64, a: [u64; 6]) -> SysResult {
    let t = thread::get(ti);
    match nr {
        sys::NOP => val(0),
        sys::DEBUG_WRITE => debug_write(a[0], a[1]),
        sys::YIELD => {
            sched::yield_current();
            val(0)
        }
        sys::THREAD_EXIT => thread_exit(ti, a[0] as i64, a[1] as u32, a[2]),
        sys::PROCESS_EXIT => {
            process::kill(pi, a[0] as i64);
            Ok(None)
        }
        sys::SLEEP => sleep(ti, a[0]),
        sys::CLOCK => match a[0] {
            clock::MONOTONIC => val(timer::monotonic_ns()),
            clock::REALTIME => val(timer::realtime_ns()),
            _ => Err(-EINVAL),
        },
        sys::GETCPU => val(t.home as u64),
        sys::GETPID => {
            let target = if a[0] == 0 {
                pi
            } else {
                cap::lookup(pi, a[0] as u32, Kind::Process, 0)?.obj as u16
            };
            val(process::get(target).pid as u64)
        }
        sys::GETTID => val(t.tid as u64),
        sys::GETRANDOM => getrandom(a[0], a[1]),
        sys::CLOCK_SET => {
            cap::lookup(pi, a[0] as u32, Kind::SysControl, rights::CONTROL)?;
            if a[1] != clock::REALTIME {
                return Err(-EINVAL);
            }
            timer::set_realtime(a[2]);
            val(0)
        }

        sys::HANDLE_CLOSE => cap::remove(pi, a[0] as u32).map(|_| Some(0)),
        sys::HANDLE_REVOKE => cap::revoke(pi, a[0] as u32).map(|_| Some(0)),
        sys::HANDLE_DUP => handle_dup(pi, a[0] as u32, a[1] as u16, a[2]),

        sys::MEM_MAP => mem_map(pi, a[0], a[1], a[2]),
        sys::MEM_UNMAP => mem_unmap(pi, a[0], a[1]),
        sys::MMIO_MAP => mmio_map(pi, a[0] as u32, a[1], a[2]),
        sys::MMIO_DERIVE => mmio_derive(pi, a[0] as u32, a[1], a[2]),
        sys::MEM_PROTECT => mem_protect(pi, a[0], a[1], a[2]),
        sys::SHM_CREATE => {
            let (i, gen) = shm::create(a[0], a[1])?;
            let r = rights::MAP
                | rights::WRITE
                | rights::GRANT
                | rights::DUP
                | rights::DMA
                | rights::CONTROL;
            match cap::insert(pi, Cap::new(Kind::Shm, i as u32, gen, r)) {
                Ok(h) => val(h as u64),
                Err(e) => {
                    // No reference taken: the object never existed.
                    shm::incref(i);
                    shm::decref(i);
                    Err(e)
                }
            }
        }
        sys::SHM_MAP => shm_map(pi, a[0] as u32, a[1], a[2]),
        sys::SHM_PADDR => {
            let c = cap::lookup(pi, a[0] as u32, Kind::Shm, rights::DMA)?;
            shm::paddr(c.obj as u16, a[1]).map(Some)
        }

        sys::THREAD_CREATE => thread_create(pi, a[0], a[1], a[2], a[3]),
        sys::THREAD_SET => thread_set(ti, pi, a[0] as u32, a[1], a[2]),

        sys::FUTEX_WAIT => futex::wait(ti, a[0], a[1] as u32, Timeout::from_ns(a[2])),
        sys::FUTEX_WAKE => futex::wake(a[0], a[1]).map(Some),

        sys::PROCESS_SPAWN => spawn(pi, a[0]),
        sys::PROCESS_WAIT => {
            let c = cap::lookup(pi, a[0] as u32, Kind::Process, rights::WAIT)?;
            process::wait(ti, c.obj as u16, Timeout::from_ns(a[1]))
        }
        sys::PROCESS_KILL => {
            let c = cap::lookup(pi, a[0] as u32, Kind::Process, rights::KILL)?;
            process::kill(c.obj as u16, 137);
            val(0)
        }

        sys::ENDPOINT_CREATE => {
            if a[0] & !nxk_abi::endpoint::PIPE != 0 {
                return Err(-EINVAL);
            }
            let (i, gen) = ipc::create(a[0] & nxk_abi::endpoint::PIPE != 0).ok_or(-ENOMEM)?;
            cap::insert(pi, Cap::new(Kind::Endpoint, i as u32, gen, rights::ALL))
                .map(|h| Some(h as u64))
        }
        sys::IPC_CALL => ipc::call(ti, a[0] as u32).map(|_| None),
        sys::IPC_SEND => ipc::send(ti, a[0] as u32).map(|_| None),
        sys::IPC_RECV => ipc::recv(ti, a[0] as u32).map(|_| None),
        sys::IPC_REPLY => ipc::reply(ti).map(|_| None),
        sys::IPC_REPLY_RECV => ipc::reply_recv(ti, a[0] as u32).map(|_| None),
        sys::IPC_REPLY_TO => ipc::reply_to(ti, a[0]).map(|_| None),

        sys::NOTIFY_CREATE => {
            let (i, gen) = notify::create().ok_or(-ENOMEM)?;
            cap::insert(pi, Cap::new(Kind::Notification, i as u32, gen, rights::ALL))
                .map(|h| Some(h as u64))
        }
        sys::NOTIFY_SIGNAL => {
            let c = cap::lookup(pi, a[0] as u32, Kind::Notification, rights::SIGNAL)?;
            notify::signal(c.obj as u16, a[1]);
            val(0)
        }
        sys::NOTIFY_WAIT => {
            let c = cap::lookup(pi, a[0] as u32, Kind::Notification, rights::WAIT)?;
            notify::wait(ti, c.obj as u16, Timeout::from_ns(a[1]))
        }
        sys::NOTIFY_BIND => {
            let c = cap::lookup(pi, a[0] as u32, Kind::Notification, rights::WAIT)?;
            notify::bind(c.obj as u16, ti);
            val(0)
        }

        sys::IRQ_GET => {
            cap::lookup(pi, a[0] as u32, Kind::IrqControl, 0)?;
            let intid = a[1] as u32;
            irq::claim(intid, pi)?;
            let r = rights::GRANT | rights::WAIT | rights::SIGNAL;
            match cap::insert(pi, Cap::new(Kind::Irq, intid, 0, r)) {
                Ok(h) => val(h as u64),
                Err(e) => {
                    irq::release(intid);
                    Err(e)
                }
            }
        }
        sys::IRQ_BIND => {
            let i = cap::lookup(pi, a[0] as u32, Kind::Irq, rights::WAIT)?;
            let n = cap::lookup(pi, a[1] as u32, Kind::Notification, rights::SIGNAL)?;
            irq::bind(i.obj, n.obj as u16, n.obj_gen, a[2]);
            val(0)
        }
        sys::IRQ_ACK => {
            let i = cap::lookup(pi, a[0] as u32, Kind::Irq, rights::WAIT)?;
            irq::ack(i.obj);
            val(0)
        }

        sys::SYSINFO => sysinfo(a[0], a[1], a[2]),
        sys::SYSTEM_POWER => {
            cap::lookup(pi, a[0] as u32, Kind::SysControl, rights::CONTROL)?;
            system_power(a[1])
        }

        sys::DEBUG_ATTACH => {
            let p = cap::lookup(pi, a[0] as u32, Kind::Process, rights::CONTROL)?;
            let ep = match a[1] as u32 {
                0 => None,
                h => {
                    let e = cap::lookup(pi, h, Kind::Endpoint, rights::SEND)?;
                    Some((e.obj as u16, e.obj_gen))
                }
            };
            if process::get(p.obj as u16).state != PState::Alive {
                return Err(-ESRCH);
            }
            debug::attach(p.obj as u16, ep);
            val(0)
        }
        sys::DEBUG_REGS => {
            let p = cap::lookup(pi, a[0] as u32, Kind::Process, rights::CONTROL)?;
            if a[3] != 0 {
                let r: nxk_abi::debug::Regs = uaccess::read(a[2])?;
                debug::write_regs(p.obj as u16, a[1] as u32, &r)?;
            } else {
                let r = debug::read_regs(p.obj as u16, a[1] as u32)?;
                uaccess::write(a[2], &r)?;
            }
            val(0)
        }
        sys::DEBUG_MEM => {
            let p = cap::lookup(pi, a[0] as u32, Kind::Process, rights::CONTROL)?;
            let write = a[3] >> 63 != 0;
            debug::mem(p.obj as u16, a[1], a[2], a[3] & !(1 << 63), write).map(Some)
        }
        sys::DEBUG_THREADS => {
            let p = cap::lookup(pi, a[0] as u32, Kind::Process, rights::CONTROL)?;
            debug::threads(p.obj as u16, a[1], a[2]).map(Some)
        }
        sys::DEBUG_STOP => {
            let p = cap::lookup(pi, a[0] as u32, Kind::Process, rights::CONTROL)?;
            let target = p.obj as u16;
            if a[1] == 0 {
                let mut ti = process::get(target).threads_head;
                while ti != thread::NIL {
                    let next = thread::get(ti).proc_next;
                    debug::stop(ti);
                    ti = next;
                }
            } else {
                debug::stop(process::find_thread(target, a[1] as u32).ok_or(-ESRCH)?);
            }
            val(0)
        }
        _ => Err(-ENOSYS),
    }
}

/// Machine reboot or poweroff.
fn system_power(cmd: u64) -> SysResult {
    use crate::arch::psci;
    match cmd {
        nxk_abi::power::REBOOT => {
            crate::kprintln!("[nxk] reboot");
            if psci::present() {
                psci::system_reset();
            }
            // No monitor: main SoC reset (PMC_CNTRL.MAIN_RST).
            unsafe { crate::mmio::write32(0x7000_E400, crate::mmio::read32(0x7000_E400) | 1 << 4) };
            crate::arch::park()
        }
        nxk_abi::power::OFF => {
            if psci::present() {
                crate::kprintln!("[nxk] poweroff");
                psci::system_off();
            }
            Err(-ENOSYS)
        }
        _ => Err(-EINVAL),
    }
}

fn debug_write(ptr: u64, len: u64) -> SysResult {
    chunked(len, |buf, off, n| {
        uaccess::copy_from_user(&mut buf[..n], ptr + off)?;
        crate::console::write_bytes(&buf[..n]);
        Ok(())
    })
}

fn getrandom(ptr: u64, len: u64) -> SysResult {
    chunked(len, |buf, off, n| {
        crate::kern::random::fill(&mut buf[..n]);
        let r = uaccess::copy_to_user(ptr + off, &buf[..n]);
        buf.fill(0);
        r
    })
}

/// Run `f` over `[0, len)` in 256-byte chunks; returns `len`.
fn chunked(
    len: u64,
    mut f: impl FnMut(&mut [u8; 256], u64, usize) -> Result<(), i64>,
) -> SysResult {
    let len = len.min(64 * 1024);
    let mut buf = [0u8; 256];
    let mut off = 0;
    while off < len {
        let n = (len - off).min(buf.len() as u64) as usize;
        f(&mut buf, off, n)?;
        off += n as u64;
    }
    val(len)
}

/// Exit the current thread; an optional notification fires after EL0 exit.
fn thread_exit(ti: u16, code: i64, notif: u32, bits: u64) -> SysResult {
    if notif != 0 {
        let pi = thread::get(ti).proc_idx;
        if let Ok(c) = cap::lookup(pi, notif, Kind::Notification, rights::SIGNAL) {
            notify::signal(c.obj as u16, bits);
        }
    }
    let t = thread::get(ti);
    let p = process::get(t.proc_idx);
    if p.nthreads == 1 {
        p.exit_code = code;
    }
    t.state = State::Zombie;
    Ok(None)
}

fn sleep(ti: u16, ns: u64) -> SysResult {
    let ticks = timer::ns_to_ticks(ns);
    if ticks == 0 {
        sched::yield_current();
        return val(0);
    }
    sched::sleep_until(ti, timer::counter().saturating_add(ticks));
    Ok(None)
}

fn handle_dup(pi: u16, h: u32, mask: u16, badge: u64) -> SysResult {
    let mut c = cap::lookup_any(pi, h)?;
    if c.rights & rights::DUP == 0 {
        return Err(-EACCES);
    }
    c.rights &= mask;
    if badge != 0 {
        // Only a badgeless endpoint may gain one (like `mint`).
        if c.kind != Kind::Endpoint || c.badge != 0 {
            return Err(-EINVAL);
        }
        c.badge = badge;
    }
    cap::insert(pi, c).map(|h| Some(h as u64))
}

fn page_range_ok(va: u64, len: u64) -> bool {
    len > 0
        && va % PAGE_SIZE == 0
        && len % PAGE_SIZE == 0
        && va >= USER_MIN
        && va.checked_add(len).is_some_and(|e| e <= USER_MAX)
}

fn overlaps_utcb(va: u64, len: u64) -> bool {
    let utcb_end = UTCB_BASE + MAX_THREADS_PER_PROCESS as u64 * PAGE_SIZE;
    va < utcb_end && va + len > UTCB_BASE
}

/// Page-aligned user range outside the UTCB area.
fn user_range_ok(va: u64, len: u64) -> bool {
    page_range_ok(va, len) && !overlaps_utcb(va, len)
}

/// W^X: no page is both writable and executable.
fn check_no_wx(p: u64) -> Result<(), i64> {
    if p & prot::WRITE != 0 && p & prot::EXEC != 0 {
        return Err(-EACCES);
    }
    Ok(())
}

/// Mask check for rwx-style prot bits.
fn check_prot_mask(p: u64, extra: u64) -> Result<(), i64> {
    if p & !(prot::READ | prot::WRITE | prot::EXEC | extra) != 0 {
        return Err(-EINVAL);
    }
    Ok(())
}

fn mem_map(pi: u16, addr: u64, len: u64, p: u64) -> SysResult {
    let len = pmm::align_up(len, PAGE_SIZE);
    if len == 0 || len > 1 << 32 {
        return Err(-EINVAL);
    }
    check_prot_mask(p, prot::POPULATE)?;
    check_no_wx(p)?;
    let va = place(pi, addr, len)?;
    // Fixed address: the range must be free, else failure (and its cleanup)
    // would touch already-mapped pages.
    let l1 = process::get(pi).l1;
    if addr != 0 && (0..len / PAGE_SIZE).any(|k| vm::is_mapped(l1, addr + k * PAGE_SIZE)) {
        return Err(-EEXIST);
    }
    let r = if p & prot::POPULATE != 0 {
        process::map_anon(pi, va, len, (p & !prot::POPULATE) | prot::READ)
    } else {
        process::reserve_anon(pi, va, len, p | prot::READ)
    };
    if let Err(e) = r {
        process::unmap(pi, va, len);
        return Err(e);
    }
    val(va)
}

/// `len`-byte area for a mapping: fixed `addr`, or taken from the process heap
/// (with a guard page after).
fn place(pi: u16, addr: u64, len: u64) -> Result<u64, i64> {
    if addr != 0 {
        if !user_range_ok(addr, len) {
            return Err(-EINVAL);
        }
        return Ok(addr);
    }
    let proc = process::get(pi);
    let va = proc.heap_next;
    if va + len > HEAP_END {
        return Err(-ENOMEM);
    }
    proc.heap_next += len + PAGE_SIZE;
    Ok(va)
}

fn shm_map(pi: u16, h: u32, addr: u64, p: u64) -> SysResult {
    check_prot_mask(p, 0)?;
    check_no_wx(p)?;
    let c = cap::lookup(pi, h, Kind::Shm, rights::MAP)?;
    let max = shm::check_prot(p, c.rights)?;
    let va = place(pi, addr, shm::size(c.obj as u16))?;
    shm::map(pi, c.obj as u16, va, p | prot::READ, max)?;
    val(va)
}

fn mem_protect(pi: u16, addr: u64, len: u64, p: u64) -> SysResult {
    let len = pmm::align_up(len, PAGE_SIZE);
    check_prot_mask(p, 0)?;
    check_no_wx(p)?;
    if !user_range_ok(addr, len) {
        return Err(-EINVAL);
    }
    let proc = process::get(pi);
    // Check everything before touching anything.
    let mut va = addr;
    while va < addr + len {
        let pte = vm::lookup(proc.l1, va);
        if !vm::is_mapped(proc.l1, va) {
            return Err(-ENOMEM);
        }
        if vm::is_device(pte) {
            return Err(-EACCES);
        }
        if pte & vm::SW_SHM != 0 && p & !shm::max_prot(pi, va) != 0 {
            return Err(-EACCES);
        }
        va += PAGE_SIZE;
    }
    // AArch64 has no EL0 write or execute without read.
    let p = if p != 0 { p | prot::READ } else { 0 };
    let pages = len / PAGE_SIZE;
    let mut flush = false;
    let mut va = addr;
    while va < addr + len {
        let old = vm::protect(proc.l1, va, p);
        if old & 1 != 0 {
            if pages <= 64 {
                vm::flush_page(proc.asid, va);
            } else {
                flush = true;
            }
        }
        va += PAGE_SIZE;
    }
    if flush {
        vm::flush_asid(proc.asid);
    }
    vm::publish();
    val(0)
}

fn mem_unmap(pi: u16, addr: u64, len: u64) -> SysResult {
    let len = pmm::align_up(len, PAGE_SIZE);
    // UTCBs stay put: the kernel writes them by physical address.
    if !user_range_ok(addr, len) {
        return Err(-EINVAL);
    }
    process::unmap(pi, addr, len);
    val(0)
}

fn mmio_range(c: &Cap, phys: u64, len: u64) -> Result<(), i64> {
    let base = (c.obj as u64) << 12;
    let end = base + c.extra;
    if len == 0 || phys % PAGE_SIZE != 0 || len % PAGE_SIZE != 0 || phys < base || phys + len > end
    {
        return Err(-EINVAL);
    }
    if phys < KERNEL_MMIO.end && phys + len > KERNEL_MMIO.start {
        return Err(-EPERM);
    }
    Ok(())
}

fn mmio_map(pi: u16, h: u32, phys: u64, len: u64) -> SysResult {
    let c = cap::lookup(pi, h, Kind::Mmio, rights::MAP)?;
    mmio_range(&c, phys, len)?;
    let proc = process::get(pi);
    let va = proc.mmio_next;
    if va + len > MMIO_END {
        return Err(-ENOMEM);
    }
    // Without WRITE (e.g. fuses for a driver): read-only mapping.
    let flags = vm::device_flags(c.rights & rights::WRITE != 0);
    let mut off = 0;
    while off < len {
        vm::map(proc.l1, va + off, phys + off, flags)?;
        off += PAGE_SIZE;
    }
    vm::publish();
    proc.mmio_next += len + PAGE_SIZE;
    val(va)
}

fn mmio_derive(pi: u16, h: u32, phys: u64, len: u64) -> SysResult {
    let c = cap::lookup(pi, h, Kind::Mmio, rights::DUP)?;
    mmio_range(&c, phys, len)?;
    let mut n = Cap::new(Kind::Mmio, (phys >> 12) as u32, 0, c.rights);
    n.extra = len;
    cap::insert(pi, n).map(|h| Some(h as u64))
}

fn thread_create(pi: u16, entry: u64, sp: u64, arg: u64, flags: u64) -> SysResult {
    if !(USER_MIN..USER_MAX).contains(&entry) || !(USER_MIN..=USER_MAX).contains(&sp) {
        return Err(-EFAULT);
    }
    let prio = (flags & 0xFF) as u8;
    let cpu = match (flags >> 8) & 0xFF {
        0 => None,
        n => Some(n as usize - 1),
    };
    let ti = process::create_thread(pi, entry, sp, [arg, 0, 0], prio, cpu)?;
    let gen = thread::get(ti).gen;
    let r = rights::GRANT | rights::DUP | rights::CONTROL;
    cap::insert(pi, Cap::new(Kind::Thread, ti as u32, gen, r)).map(|h| Some(h as u64))
}

/// Thread setting: `h` = 0 names the calling thread.
fn thread_set(ti: u16, pi: u16, h: u32, what: u64, v: u64) -> SysResult {
    use nxk_abi::thread as abi;
    let target = if h == 0 {
        ti
    } else {
        cap::lookup(pi, h, Kind::Thread, rights::CONTROL)?.obj as u16
    };
    match what {
        abi::SET_PRIO => {
            let p = match v {
                0 => abi::DEFAULT_PRIO,
                1..=30 => v as u8,
                _ => return Err(-EINVAL),
            };
            sched::set_prio(target, p);
        }
        abi::SET_AFFINITY => {
            let cpu = if v == abi::ANY_CPU {
                thread::ANY_CPU
            } else if (v as usize) < percpu::NR_CPUS && percpu::get(v as usize).online {
                v as u8
            } else {
                return Err(-EINVAL);
            };
            sched::set_affinity(target, cpu);
        }
        _ => return Err(-EINVAL),
    }
    val(0)
}

fn spawn(pi: u16, args_ptr: u64) -> SysResult {
    let a: SpawnArgs = uaccess::read(args_ptr)?;
    if a.elf_len == 0 || a.elf_len > 32 << 20 || a.name_len > 15 || a.grants_len > 16 {
        return Err(-EINVAL);
    }
    let mut name = [0u8; 15];
    uaccess::copy_from_user(&mut name[..a.name_len as usize], a.name_ptr)?;

    let child = process::create(&name[..a.name_len as usize], pi)?;
    let r = (|| -> Result<(), i64> {
        let src = UserSource {
            va: a.elf_ptr,
            len: a.elf_len,
        };
        let loaded = elf::load(&src, child)?;
        let strings = exec::Strings::User {
            va: a.strings_ptr,
            len: a.strings_len,
        };
        let start = exec::prepare(child, &src, &loaded, strings, a.argc, a.envc)?;
        for k in 0..a.grants_len {
            let g: HandleGrant =
                uaccess::read(a.grants_ptr + k * core::mem::size_of::<HandleGrant>() as u64)?;
            cap::grant(pi, g.handle, child, Some(g.slot as usize))?;
        }
        let cpu = (a.cpu >= 0).then_some(a.cpu as usize);
        let args = [a.arg0, a.arg1, start.block];
        let ti = process::create_thread(child, loaded.entry, start.sp, args, a.prio as u8, cpu)?;
        if a.flags & nxk_abi::spawn::SUSPENDED != 0 {
            // Parked before its first instruction, until the debugger.
            thread::get(ti).stop_request = true;
        }
        Ok(())
    })();
    if let Err(e) = r {
        process::abort_unstarted(child);
        return Err(e);
    }
    let gen = process::get(child).gen;
    let r = rights::WAIT | rights::KILL | rights::GRANT | rights::DUP | rights::CONTROL;
    cap::insert(pi, Cap::new(Kind::Process, child as u32, gen, r)).map(|h| Some(h as u64))
}

fn sysinfo(kind: u64, buf: u64, len: u64) -> SysResult {
    match kind {
        info::PROCESSES => {
            let sz = core::mem::size_of::<ProcInfo>() as u64;
            let mut n = 0u64;
            for i in 0..process::capacity() as u16 {
                let p = process::get(i);
                if p.state == PState::Free {
                    continue;
                }
                if (n + 1) * sz <= len {
                    let mut pi = ProcInfo {
                        pid: p.pid,
                        threads: p.nthreads as u32,
                        pages: p.pages,
                        name: [0; 16],
                        state: if p.state == PState::Alive {
                            info::PSTATE_ALIVE
                        } else {
                            info::PSTATE_ZOMBIE
                        },
                        parent: if p.parent == thread::NIL {
                            0
                        } else {
                            process::get(p.parent).pid
                        },
                        cpu_ns: timer::ticks_to_ns(p.cpu_time),
                    };
                    pi.name.copy_from_slice(&p.name);
                    uaccess::write(buf + n * sz, &pi)?;
                }
                n += 1;
            }
            val(n)
        }
        info::MEMORY => {
            let (total, free, kernel) = pmm::stats();
            let m = MemInfo {
                total_pages: total,
                free_pages: free,
                kernel_pages: kernel,
            };
            if len < core::mem::size_of::<MemInfo>() as u64 {
                return Err(-EINVAL);
            }
            uaccess::write(buf, &m)?;
            val(1)
        }
        info::CPUS => {
            let sz = core::mem::size_of::<CpuInfo>() as u64;
            for c in 0..percpu::NR_CPUS {
                if (c as u64 + 1) * sz > len {
                    break;
                }
                let pc = percpu::get(c);
                // Current slice included, for a long-idle core.
                let (mut idle, mut busy) = (pc.stats.idle_time, pc.stats.busy_time);
                if pc.online {
                    let dt = timer::counter().saturating_sub(pc.acct_start);
                    if pc.acct_thread == thread::NIL {
                        idle += dt;
                    } else {
                        busy += dt;
                    }
                }
                let ci = CpuInfo {
                    online: pc.online as u64,
                    switches: pc.stats.switches,
                    syscalls: pc.stats.syscalls,
                    irqs: pc.stats.irqs,
                    idle_ns: timer::ticks_to_ns(idle),
                    busy_ns: timer::ticks_to_ns(busy),
                };
                uaccess::write(buf + c as u64 * sz, &ci)?;
            }
            val(percpu::NR_CPUS as u64)
        }
        _ => Err(-EINVAL),
    }
}
