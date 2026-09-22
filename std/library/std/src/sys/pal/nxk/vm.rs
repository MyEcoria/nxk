//! Anonymous mappings with address reuse: freed ranges are cached and remapped fixed.
//! Invariant: listed ranges are free; each live mapping is followed by an off-list guard page.

use super::abi::layout::PAGE_SIZE;
use super::abi::prot;
use super::syscall;
use crate::cell::UnsafeCell;
use crate::sys::sync::Mutex;

pub const PAGE: usize = PAGE_SIZE as usize;
const SLOTS: usize = 256;
const RW: u64 = prot::READ | prot::WRITE;

struct Ranges {
    /// Free ranges sorted by address, never adjacent (merged).
    start: [usize; SLOTS],
    len: [usize; SLOTS],
    n: usize,
}

struct State {
    lock: Mutex,
    ranges: UnsafeCell<Ranges>,
}

// SAFETY: `ranges` only touched under `lock`.
unsafe impl Sync for State {}

static STATE: State = State {
    lock: Mutex::new(),
    ranges: UnsafeCell::new(Ranges { start: [0; SLOTS], len: [0; SLOTS], n: 0 }),
};

fn with<R>(f: impl FnOnce(&mut Ranges) -> R) -> R {
    STATE.lock.lock();
    // SAFETY: exclusive access under the lock.
    let r = f(unsafe { &mut *STATE.ranges.get() });
    unsafe { STATE.lock.unlock() };
    r
}

/// Pop `need` bytes off a free range (first fit).
fn take(need: usize) -> Option<usize> {
    with(|r| {
        let i = (0..r.n).find(|&i| r.len[i] >= need)?;
        let a = r.start[i];
        if r.len[i] == need {
            r.start.copy_within(i + 1..r.n, i);
            r.len.copy_within(i + 1..r.n, i);
            r.n -= 1;
        } else {
            r.start[i] += need;
            r.len[i] -= need;
        }
        Some(a)
    })
}

/// Return a free (unmapped) range to the list.
pub fn record(start: usize, len: usize) {
    if len == 0 {
        return;
    }
    with(|r| {
        let i = r.start[..r.n].partition_point(|&s| s < start);
        let joins_prev = i > 0 && r.start[i - 1] + r.len[i - 1] == start;
        let joins_next = i < r.n && start + len == r.start[i];
        match (joins_prev, joins_next) {
            (true, true) => {
                r.len[i - 1] += len + r.len[i];
                r.start.copy_within(i + 1..r.n, i);
                r.len.copy_within(i + 1..r.n, i);
                r.n -= 1;
            }
            (true, false) => r.len[i - 1] += len,
            (false, true) => {
                r.start[i] = start;
                r.len[i] += len;
            }
            (false, false) => {
                // List full: range dropped (only the address is lost;
                // pages already went back to the kernel).
                if r.n < SLOTS {
                    r.start.copy_within(i..r.n, i + 1);
                    r.len.copy_within(i..r.n, i + 1);
                    r.start[i] = start;
                    r.len[i] = len;
                    r.n += 1;
                }
            }
        }
    })
}

/// Map `len` read/write bytes (zeroed demand pages) plus a guard page.
pub fn map(len: usize) -> Option<*mut u8> {
    let len = len.checked_next_multiple_of(PAGE)?;
    if let Some(a) = take(len.checked_add(PAGE)?) {
        if syscall::mem_map_at(a as u64, len as u64, RW) >= 0 {
            return Some(a as *mut u8);
        }
        // Should not happen: the range is just dropped.
    }
    let r = syscall::mem_map(len as u64, RW);
    if r < 0 { None } else { Some(r as *mut u8) }
}

/// Free a `map` mapping (same length) plus guard. # Safety: no further access.
pub unsafe fn unmap(p: *mut u8, len: usize) {
    let len = len.next_multiple_of(PAGE);
    syscall::mem_unmap(p as u64, len as u64);
    record(p as usize, len + PAGE);
}

/// Unmap `[start, start + len)` and recycle the range. # Safety: no further access.
pub unsafe fn release(start: usize, len: usize) {
    if len > 0 {
        syscall::mem_unmap(start as u64, len as u64);
        record(start, len);
    }
}
