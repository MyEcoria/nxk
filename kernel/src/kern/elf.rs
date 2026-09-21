//! Chargeur ELF : executables statiques AArch64, a adresse fixe ou
//! independants de la position (static-PIE).
//!
//! Un static-PIE est lie a partir de 0 : le noyau tire une base aleatoire
//! (ASLR, 20 bits d'entropie par pas de 64 KiB), y projette les segments puis
//! applique les relocations `R_AARCH64_RELATIVE` (`*(base+off) = base+addend`)
//! directement dans les pages physiques du processus. Les donnees relogees
//! peuvent donc vivre dans un segment en lecture seule (RELRO complet) : elles
//! sont figees avant que le programme ne demarre.
//!
//! Les octets sont lus a la source (image du noyau ou memoire de l'appelant,
//! via `uaccess`) et ecrits directement dans les pages du nouveau processus :
//! une seule copie, aucun tampon intermediaire.

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
        if off.checked_add(dst.len() as u64).is_none_or(|e| e > self.len) {
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
/// Granularite de la base aleatoire.
const ASLR_ALIGN: u64 = 64 * 1024;

/// Resultat du chargement.
pub struct Loaded {
    pub entry: u64,
    /// Base de chargement (0 : executable a adresse fixe).
    pub base: u64,
    pub phoff: u64,
    pub phnum: u16,
}

fn read_struct<T: Copy + Default>(src: &dyn Source, off: u64) -> Result<T, i64> {
    let mut v = T::default();
    let bytes = unsafe { core::slice::from_raw_parts_mut(&mut v as *mut T as *mut u8, core::mem::size_of::<T>()) };
    src.read(off, bytes)?;
    Ok(v)
}

pub fn phdr(src: &dyn Source, phoff: u64, k: u16) -> Result<Phdr, i64> {
    read_struct(src, phoff + k as u64 * core::mem::size_of::<Phdr>() as u64)
}

/// Charge l'executable dans le processus `pi`.
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
    if eh.machine != EM_AARCH64 || eh.phentsize as usize != core::mem::size_of::<Phdr>() || eh.phnum > MAX_PHDRS {
        return Err(-ENOEXEC);
    }

    // Etendue des segments, pour placer une image independante de la position.
    let mut span = 0u64;
    for k in 0..eh.phnum {
        let ph = phdr(src, eh.phoff, k)?;
        if ph.ptype == PT_LOAD {
            span = span.max(ph.vaddr.checked_add(ph.memsz).ok_or(-ENOEXEC)?);
        }
    }
    let base = if pie {
        let room = (HEAP_BASE - TEXT_BASE).checked_sub(pmm::align_up(span, ASLR_ALIGN)).ok_or(-ENOEXEC)?;
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
            return Err(-ENOEXEC); // W^X, y compris pour les executables
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
            // Portion du fichier qui tombe dans cette page.
            let lo = va.max(vaddr);
            let hi = (va + PAGE_SIZE).min(file_end);
            if lo < hi {
                let dst = unsafe { core::slice::from_raw_parts_mut((pa + (lo - va)) as *mut u8, (hi - lo) as usize) };
                src.read(ph.offset + (lo - vaddr), dst)?;
            }
            va += PAGE_SIZE;
        }
    }
    if pie {
        relocate(src, &eh, base, l1)?;
    }
    // Code eventuellement retouche par les relocations : le cache
    // d'instructions ne doit voir que la version finale.
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
    Ok(Loaded { entry: base + eh.entry, base, phoff: eh.phoff, phnum: eh.phnum })
}

/// Decalage dans le fichier d'une adresse (non relogee) d'un segment.
fn file_offset(src: &dyn Source, eh: &Ehdr, vaddr: u64) -> Result<u64, i64> {
    for k in 0..eh.phnum {
        let ph = phdr(src, eh.phoff, k)?;
        if ph.ptype == PT_LOAD && vaddr >= ph.vaddr && vaddr < ph.vaddr + ph.filesz {
            return Ok(ph.offset + (vaddr - ph.vaddr));
        }
    }
    Err(-ENOEXEC)
}

/// Applique les relocations d'un static-PIE charge a `base`.
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
            DT_REL | DT_RELR => return Err(-ENOEXEC), // formats non pris en charge
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
                poke(l1, base.checked_add(r.offset).ok_or(-ENOEXEC)?, &value.to_le_bytes())?;
            }
            _ => return Err(-ENOEXEC),
        }
    }
    Ok(())
}

/// Ecrit dans une page deja projetee du processus, quelles que soient ses
/// protections (par l'adresse physique).
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
