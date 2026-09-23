//! Per-process capability tables: `(gen << 16) | slot` handles, O(1).

use nxk_abi::err::{EACCES, EBADF, ENOSPC};

use crate::kern::process;
use crate::kern::{ipc, irq, notify, shm};
use crate::mm::pmm;

/// Initial table: one page (128 entries), doubled on demand.
const INITIAL_CAPS: usize = 128;
pub const MAX_CAPS: usize = 0x10000;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum Kind {
    Empty = 0,
    Process,
    Thread,
    Endpoint,
    Notification,
    Irq,
    IrqControl,
    Mmio,
    /// System control (wall clock).
    SysControl,
    /// Shared-memory object.
    Shm,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct Cap {
    pub kind: Kind,
    pub _r: u8,
    pub rights: u16,
    pub slot_gen: u16,
    pub obj_gen: u16,
    /// Object index; for Mmio, physical base >> 12; for Irq, INTID.
    pub obj: u32,
    pub _pad: u32,
    pub badge: u64,
    /// Mmio: range size.
    pub extra: u64,
}

const _: () = assert!(core::mem::size_of::<Cap>() == 32);

impl Cap {
    pub const fn new(kind: Kind, obj: u32, obj_gen: u16, rights: u16) -> Self {
        Self {
            kind,
            _r: 0,
            rights,
            slot_gen: 0,
            obj_gen,
            obj,
            _pad: 0,
            badge: 0,
            extra: 0,
        }
    }
}

#[inline(always)]
fn table(p: u16) -> &'static mut [Cap] {
    let pr = process::get(p);
    unsafe { core::slice::from_raw_parts_mut(pr.caps as *mut Cap, pr.caps_len as usize) }
}

fn order_for_len(len: usize) -> usize {
    pmm::order_for((len * core::mem::size_of::<Cap>()) as u64)
}

pub fn alloc_table() -> Option<(u64, u32)> {
    pmm::alloc_zeroed(order_for_len(INITIAL_CAPS)).map(|pa| (pa, INITIAL_CAPS as u32))
}

pub fn free_table(pa: u64, len: u32) {
    pmm::free(pa, order_for_len(len as usize));
}

/// Double the table (entries copied, generations included).
fn grow(p: u16) -> Result<(), i64> {
    let pr = process::get(p);
    let old_len = pr.caps_len as usize;
    if old_len >= MAX_CAPS {
        return Err(-ENOSPC);
    }
    let new_len = (old_len * 2).min(MAX_CAPS);
    let pa = pmm::alloc_zeroed(order_for_len(new_len)).ok_or(-ENOSPC)?;
    unsafe { core::ptr::copy_nonoverlapping(pr.caps as *const Cap, pa as *mut Cap, old_len) };
    free_table(pr.caps, pr.caps_len);
    pr.caps = pa;
    pr.caps_len = new_len as u32;
    Ok(())
}

#[inline(always)]
pub const fn handle(slot: usize, gen: u16) -> u32 {
    ((gen as u32) << 16) | slot as u32
}

/// Does the named object still exist (same generation)?
fn object_alive(c: &Cap) -> bool {
    match c.kind {
        Kind::Endpoint => ipc::alive(c.obj as u16, c.obj_gen),
        Kind::Notification => notify::alive(c.obj as u16, c.obj_gen),
        Kind::Process => process::alive(c.obj as u16, c.obj_gen),
        Kind::Shm => shm::alive(c.obj as u16, c.obj_gen),
        Kind::Thread => {
            let t = crate::kern::thread::get(c.obj as u16);
            t.gen == c.obj_gen && t.state != crate::kern::thread::State::Free
        }
        _ => true,
    }
}

/// Resolve a handle: expected type, required rights, object still alive.
#[inline]
pub fn lookup(p: u16, h: u32, kind: Kind, need: u16) -> Result<Cap, i64> {
    let slot = (h & 0xFFFF) as usize;
    let t = table(p);
    if slot == 0 || slot >= t.len() {
        return Err(-EBADF);
    }
    let c = t[slot];
    if c.kind == Kind::Empty || c.slot_gen != (h >> 16) as u16 || c.kind != kind {
        return Err(-EBADF);
    }
    if c.rights & need != need {
        return Err(-EACCES);
    }
    if !object_alive(&c) {
        return Err(-EBADF);
    }
    Ok(c)
}

/// Resolve a handle with no type constraint (dup, transfer).
pub fn lookup_any(p: u16, h: u32) -> Result<Cap, i64> {
    let slot = (h & 0xFFFF) as usize;
    let t = table(p);
    if slot == 0 || slot >= t.len() {
        return Err(-EBADF);
    }
    let c = t[slot];
    if c.kind == Kind::Empty || c.slot_gen != (h >> 16) as u16 || !object_alive(&c) {
        return Err(-EBADF);
    }
    Ok(c)
}

fn incref(c: &Cap) {
    match c.kind {
        Kind::Endpoint => ipc::incref(c.obj as u16, c.rights),
        Kind::Notification => notify::incref(c.obj as u16),
        Kind::Process => process::incref(c.obj as u16),
        Kind::Shm => shm::incref(c.obj as u16),
        Kind::Irq => irq::incref(c.obj),
        _ => {}
    }
}

/// Drop a capability reference. A revoked capability (stale generation)
/// holds none: revocation already zeroed the count.
fn decref(c: &Cap) {
    match c.kind {
        Kind::Endpoint if ipc::alive(c.obj as u16, c.obj_gen) => {
            ipc::decref(c.obj as u16, c.rights)
        }
        Kind::Notification if notify::alive(c.obj as u16, c.obj_gen) => {
            notify::decref(c.obj as u16)
        }
        Kind::Shm if shm::alive(c.obj as u16, c.obj_gen) => shm::decref(c.obj as u16),
        Kind::Process => process::decref(c.obj as u16),
        Kind::Irq => irq::decref(c.obj),
        _ => {}
    }
}

/// Revoke all other capabilities naming `h`'s object: the object moves to a
/// new generation, only `h` (updated in place) stays valid.
pub fn revoke(p: u16, h: u32) -> Result<(), i64> {
    let c = lookup_any(p, h)?;
    if c.rights & nxk_abi::rights::CONTROL == 0 {
        return Err(-EACCES);
    }
    let gen = match c.kind {
        Kind::Endpoint => ipc::revoke(c.obj as u16, c.rights),
        Kind::Notification => notify::revoke(c.obj as u16),
        _ => return Err(-nxk_abi::err::EINVAL),
    };
    table(p)[(h & 0xFFFF) as usize].obj_gen = gen;
    Ok(())
}

/// Store a capability in a free slot and take a reference.
pub fn insert(p: u16, cap: Cap) -> Result<u32, i64> {
    let hint = (process::get(p).cap_hint as usize).max(1);
    let found = {
        let t = table(p);
        (hint..t.len()).find(|&s| t[s].kind == Kind::Empty)
    };
    let slot = match found {
        Some(s) => s,
        None => {
            let old = process::get(p).caps_len as usize;
            grow(p)?;
            old
        }
    };
    process::get(p).cap_hint = slot as u32 + 1;
    insert_at(p, slot, cap)
}

/// Store a capability at a fixed slot (boot handles).
pub fn insert_at(p: u16, slot: usize, mut cap: Cap) -> Result<u32, i64> {
    if slot == 0 || slot >= MAX_CAPS {
        return Err(-ENOSPC);
    }
    while slot >= process::get(p).caps_len as usize {
        grow(p)?;
    }
    let t = table(p);
    if t[slot].kind != Kind::Empty {
        return Err(-ENOSPC);
    }
    cap.slot_gen = t[slot].slot_gen;
    incref(&cap);
    t[slot] = cap;
    Ok(handle(slot, cap.slot_gen))
}

/// Close a handle; the object dies with its last reference.
pub fn remove(p: u16, h: u32) -> Result<(), i64> {
    let slot = (h & 0xFFFF) as usize;
    let t = table(p);
    if slot == 0 || slot >= t.len() {
        return Err(-EBADF);
    }
    let c = t[slot];
    if c.kind == Kind::Empty || c.slot_gen != (h >> 16) as u16 {
        return Err(-EBADF);
    }
    t[slot].kind = Kind::Empty;
    t[slot].slot_gen = c.slot_gen.wrapping_add(1);
    let pr = process::get(p);
    if (slot as u32) < pr.cap_hint {
        pr.cap_hint = slot as u32;
    }
    decref(&c);
    Ok(())
}

/// Close everything (process exit).
pub fn close_all(p: u16) {
    for slot in 1..process::get(p).caps_len as usize {
        let c = table(p)[slot];
        if c.kind != Kind::Empty {
            table(p)[slot].kind = Kind::Empty;
            decref(&c);
        }
    }
}

/// Copy a capability between processes (transfer or spawn). Needs GRANT on
/// the source.
pub fn grant(from: u16, h: u32, to: u16, slot: Option<usize>) -> Result<u32, i64> {
    let c = lookup_any(from, h)?;
    if c.rights & nxk_abi::rights::GRANT == 0 {
        return Err(-EACCES);
    }
    match slot {
        Some(s) => insert_at(to, s, c),
        None => insert(to, c),
    }
}
