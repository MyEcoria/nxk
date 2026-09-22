//! Kernel access to user memory via `AT S1E0R/W` plus identity view.
//! EL0 addresses are never dereferenced; kernel faults impossible.

use nxk_abi::err::EFAULT;
use nxk_abi::layout::{PAGE_SIZE, USER_MAX, USER_MIN};

/// Physical address of `va` if EL0 may touch it (`write` for write access).
#[inline]
pub fn translate(va: u64, write: bool) -> Option<u64> {
    if !(USER_MIN..USER_MAX).contains(&va) {
        return None;
    }
    let par: u64;
    unsafe {
        if write {
            core::arch::asm!("at s1e0w, {va}", "isb", "mrs {par}, par_el1",
                va = in(reg) va, par = out(reg) par, options(nostack));
        } else {
            core::arch::asm!("at s1e0r, {va}", "isb", "mrs {par}, par_el1",
                va = in(reg) va, par = out(reg) par, options(nostack));
        }
    }
    if par & 1 != 0 {
        return None;
    }
    Some((par & 0x0000_FFFF_FFFF_F000) | (va & (PAGE_SIZE - 1)))
}

/// Like `translate`, first populating a reserved-but-untouched page.
pub fn translate_populate(va: u64, write: bool) -> Option<u64> {
    translate(va, write).or_else(|| {
        if crate::kern::process::populate_current(va) {
            translate(va, write)
        } else {
            None
        }
    })
}

fn copy_pages(
    va: u64,
    len: usize,
    write: bool,
    mut f: impl FnMut(u64, usize, usize),
) -> Result<(), i64> {
    let end = va.checked_add(len as u64).ok_or(-EFAULT)?;
    if end > USER_MAX {
        return Err(-EFAULT);
    }
    let mut done = 0usize;
    let mut cur = va;
    while done < len {
        let in_page = (PAGE_SIZE - (cur & (PAGE_SIZE - 1))) as usize;
        let n = in_page.min(len - done);
        let pa = match translate(cur, write) {
            Some(pa) => pa,
            // Reserved-but-untouched page: populate and retry.
            None if crate::kern::process::populate_current(cur) => {
                translate(cur, write).ok_or(-EFAULT)?
            }
            None => return Err(-EFAULT),
        };
        f(pa, done, n);
        done += n;
        cur += n as u64;
    }
    Ok(())
}

pub fn copy_from_user(dst: &mut [u8], va: u64) -> Result<(), i64> {
    let len = dst.len();
    copy_pages(va, len, false, |pa, off, n| unsafe {
        core::ptr::copy_nonoverlapping(pa as *const u8, dst.as_mut_ptr().add(off), n)
    })
}

pub fn copy_to_user(va: u64, src: &[u8]) -> Result<(), i64> {
    copy_pages(va, src.len(), true, |pa, off, n| unsafe {
        core::ptr::copy_nonoverlapping(src.as_ptr().add(off), pa as *mut u8, n)
    })
}

/// Read a `Copy` struct from user space.
pub fn read<T: Copy + Default>(va: u64) -> Result<T, i64> {
    let mut v = T::default();
    let bytes = unsafe {
        core::slice::from_raw_parts_mut(&mut v as *mut T as *mut u8, core::mem::size_of::<T>())
    };
    copy_from_user(bytes, va)?;
    Ok(v)
}

pub fn write<T: Copy>(va: u64, v: &T) -> Result<(), i64> {
    let bytes = unsafe {
        core::slice::from_raw_parts(v as *const T as *const u8, core::mem::size_of::<T>())
    };
    copy_to_user(va, bytes)
}
