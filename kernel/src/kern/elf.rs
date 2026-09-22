//! ELF loader for static and static-PIE AArch64 executables.
//! Random base plus in-place RELATIVE relocs; single copy.

use nxk_abi::err::{EFAULT, EINVAL, ENOEXEC, ENOMEM};
use nxk_abi::layout::{HEAP_BASE, PAGE_SIZE, TEXT_BASE, USER_MIN};
use nxk_abi::prot;

use crate::kern::{process, random};
use crate::mm::{pmm, uaccess, vm};

pub trait Source {
    fn len(&self) -> u64;
    fn read(&self, off: u64, dst: &mut [u8]) -> Result<(), i64>;
}

pub struct KernelSource(pub &'static [u8]);

impl Source for KernelSource {
    fn len(&self) -> u64 {
        self.0.len() as u64
    }
    fn read(&self, off: u64, dst: &mut [u8]) -> Result<(), i64> {
        let end = off.checked_add(dst.len() as u64).ok_or(-EFAULT)?;
        if end > self.len() {
            return Err(-ENOEXEC);
        }
        dst.copy_from_slice(&self.0[off as usize..end as usize]);
        Ok(())
    }
}

pub struct UserSource {
    pub va: u64,
    pub len: u64,
}

impl Source for UserSource {
    fn len(&self) -> u64 {
        self.len
    }
    fn read(&self, off: u64, dst: &mut [u8]) -> Result<(), i64> {
        if off
            .checked_add(dst.len() as u64)
            .is_none_or(|e| e > self.len)
        {
            return Err(-ENOEXEC);
        }
        uaccess::copy_from_user(dst, self.va + off)
    }
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Ehdr {
    ident: [u8; 16],
    etype: u16,
    machine: u16,
    version: u32,
    entry: u64,
    phoff: u64,
    shoff: u64,
    flags: u32,
    ehsize: u16,
    phentsize: u16,
    phnum: u16,
    shentsize: u16,
    shnum: u16,
    shstrndx: u16,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct Phdr {
    pub ptype: u32,
    pub pflags: u32,
    pub offset: u64,
    pub vaddr: u64,
    pub paddr: u64,
    pub filesz: u64,
    pub memsz: u64,
    pub align: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Dyn {
    tag: i64,
    val: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Rela {
    offset: u64,
    info: u64,
    addend: i64,
}

const PT_LOAD: u32 = 1;
const PT_DYNAMIC: u32 = 2;
const PF_X: u32 = 1;
const PF_W: u32 = 2;
const EM_AARCH64: u16 = 183;
const ET_EXEC: u16 = 2;
const ET_DYN: u16 = 3;
const DT_NULL: i64 = 0;
const DT_RELA: i64 = 7;
const DT_RELASZ: i64 = 8;
const DT_RELAENT: i64 = 9;
const DT_REL: i64 = 17;
const DT_RELR: i64 = 36;
const R_AARCH64_NONE: u64 = 0;
const R_AARCH64_RELATIVE: u64 = 1027;
pub const MAX_PHDRS: u16 = 16;
/// Random base granularity.
const ASLR_ALIGN: u64 = 64 * 1024;

/// Load result.
pub struct Loaded {
    pub entry: u64,
    /// Load base (0: fixed-address executable).
    pub base: u64,
    pub phoff: u64,
    pub phnum: u16,
}

fn read_struct<T: Copy + Default>(src: &dyn Source, off: u64) -> Result<T, i64> {
    let mut v = T::default();
    let bytes = unsafe {
        core::slice::from_raw_parts_mut(&mut v as *mut T as *mut u8, core::mem::size_of::<T>())
    };
    src.read(off, bytes)?;
    Ok(v)
}

pub fn phdr(src: &dyn Source, phoff: u64, k: u16) -> Result<Phdr, i64> {
    read_struct(src, phoff + k as u64 * core::mem::size_of::<Phdr>() as u64)
}

/// Load the executable into process `pi`.
pub fn load(src: &dyn Source, pi: u16) -> Result<Loaded, i64> {
    let eh: Ehdr = read_struct(src, 0)?;
    if &eh.ident[..4] != b"\x7fELF" || eh.ident[4] != 2 || eh.ident[5] != 1 {
        return Err(-ENOEXEC);
    }
    let pie = match eh.etype {
        ET_EXEC => false,
        ET_DYN => true,
        _ => return Err(-ENOEXEC),
    };
    if eh.machine != EM_AARCH64
        || eh.phentsize as usize != core::mem::size_of::<Phdr>()
        || eh.phnum > MAX_PHDRS
    {
        return Err(-ENOEXEC);
    }

    // Segment span, to place a position-independent image.
    let mut span = 0u64;
    for k in 0..eh.phnum {
        let ph = phdr(src, eh.phoff, k)?;
        if ph.ptype == PT_LOAD {
            span = span.max(ph.vaddr.checked_add(ph.memsz).ok_or(-ENOEXEC)?);
        }
    }
    let base = if pie {
        let room = (HEAP_BASE - TEXT_BASE)
            .checked_sub(pmm::align_up(span, ASLR_ALIGN))
            .ok_or(-ENOEXEC)?;
        TEXT_BASE + (random::next_u64() % (room / ASLR_ALIGN + 1)) * ASLR_ALIGN
    } else {
        0
    };

    let l1 = process::get(pi).l1;
    for k in 0..eh.phnum {
        let ph = phdr(src, eh.phoff, k)?;
        if ph.ptype != PT_LOAD || ph.memsz == 0 {
            continue;
        }
        let vaddr = base.checked_add(ph.vaddr).ok_or(-ENOEXEC)?;
        let end = vaddr.checked_add(ph.memsz).ok_or(-EINVAL)?;
        if ph.filesz > ph.memsz || vaddr < USER_MIN || end > HEAP_BASE {
            return Err(-ENOEXEC);
        }
        if ph.pflags & PF_W != 0 && ph.pflags & PF_X != 0 {
            return Err(-ENOEXEC);
        }
        let mut p = prot::READ;
        if ph.pflags & PF_W != 0 {
            p |= prot::WRITE;
        }
        if ph.pflags & PF_X != 0 {
            p |= prot::EXEC;
        }
        let flags = vm::user_flags(p) | vm::SW_OWNED;
        let file_end = vaddr + ph.filesz;

        let mut va = vaddr & !(PAGE_SIZE - 1);
        while va < end {
            let pa = pmm::alloc_zeroed(0).ok_or(-ENOMEM)?;
            if let Err(e) = vm::map(l1, va, pa, flags) {
                pmm::free(pa, 0);
                return Err(e);
            }
            process::get(pi).pages += 1;
            // File portion falling in this page.
            let lo = va.max(vaddr);
            let hi = (va + PAGE_SIZE).min(file_end);
            if lo < hi {
                let dst = unsafe {
                    core::slice::from_raw_parts_mut((pa + (lo - va)) as *mut u8, (hi - lo) as usize)
                };
                src.read(ph.offset + (lo - vaddr), dst)?;
            }
            va += PAGE_SIZE;
        }
    }
    if pie {
        relocate(src, &eh, base, l1)?;
    }
    // Code possibly patched by relocations: the instruction cache must only
    // see the final version.
    for k in 0..eh.phnum {
        let ph = phdr(src, eh.phoff, k)?;
        if ph.ptype == PT_LOAD && ph.pflags & PF_X != 0 {
            let mut va = (base + ph.vaddr) & !(PAGE_SIZE - 1);
            while va < base + ph.vaddr + ph.memsz {
                vm::sync_icache(vm::pte_addr(vm::lookup(l1, va)), PAGE_SIZE);
                va += PAGE_SIZE;
            }
        }
    }
    vm::publish();
    Ok(Loaded {
        entry: base + eh.entry,
        base,
        phoff: eh.phoff,
        phnum: eh.phnum,
    })
}

/// File offset of a (non-relocated) segment address.
fn file_offset(src: &dyn Source, eh: &Ehdr, vaddr: u64) -> Result<u64, i64> {
    for k in 0..eh.phnum {
        let ph = phdr(src, eh.phoff, k)?;
        if ph.ptype == PT_LOAD && vaddr >= ph.vaddr && vaddr < ph.vaddr + ph.filesz {
            return Ok(ph.offset + (vaddr - ph.vaddr));
        }
    }
    Err(-ENOEXEC)
}

/// Apply relocations of a static-PIE loaded at `base`.
fn relocate(src: &dyn Source, eh: &Ehdr, base: u64, l1: u64) -> Result<(), i64> {
    let mut dynamic = None;
    for k in 0..eh.phnum {
        let ph = phdr(src, eh.phoff, k)?;
        if ph.ptype == PT_DYNAMIC {
            dynamic = Some(ph);
        }
    }
    let Some(dph) = dynamic else { return Ok(()) };
    let (mut rela, mut relasz, mut relaent) = (0u64, 0u64, 24u64);
    let n = dph.filesz / 16;
    for k in 0..n.min(512) {
        let d: Dyn = read_struct(src, dph.offset + k * 16)?;
        match d.tag {
            DT_NULL => break,
            DT_RELA => rela = d.val,
            DT_RELASZ => relasz = d.val,
            DT_RELAENT => relaent = d.val,
            DT_REL | DT_RELR => return Err(-ENOEXEC),
            _ => {}
        }
    }
    if relasz == 0 {
        return Ok(());
    }
    if relaent != 24 || relasz % 24 != 0 || relasz / 24 > 1 << 20 {
        return Err(-ENOEXEC);
    }
    let off = file_offset(src, eh, rela)?;
    for k in 0..relasz / 24 {
        let r: Rela = read_struct(src, off + k * 24)?;
        match r.info & 0xFFFF_FFFF {
            R_AARCH64_NONE => {}
            R_AARCH64_RELATIVE => {
                let value = base.wrapping_add(r.addend as u64);
                poke(
                    l1,
                    base.checked_add(r.offset).ok_or(-ENOEXEC)?,
                    &value.to_le_bytes(),
                )?;
            }
            _ => return Err(-ENOEXEC),
        }
    }
    Ok(())
}

/// Write into an already-mapped process page, whatever its protections (via
/// the physical address).
fn poke(l1: u64, va: u64, bytes: &[u8]) -> Result<(), i64> {
    for (k, &b) in bytes.iter().enumerate() {
        let a = va + k as u64;
        let pte = vm::lookup(l1, a & !(PAGE_SIZE - 1));
        if pte & 1 == 0 {
            return Err(-ENOEXEC);
        }
        unsafe { *((vm::pte_addr(pte) + (a & (PAGE_SIZE - 1))) as *mut u8) = b };
    }
    Ok(())
}
