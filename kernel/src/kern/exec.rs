//! Preparation d'un programme : pile principale et bloc de demarrage.
//!
//! Le bloc (`nxk_abi::startup::Block`) est ecrit en haut de la pile du thread
//! principal, suivi d'une copie des en-tetes de programme et des chaines
//! (arguments puis environnement). Le programme le recoit dans x2 : c'est
//! l'equivalent de argv/envp/auxv, et ce qu'il faut a une bibliotheque
//! standard pour ses arguments, son environnement et son stockage local de
//! thread (en-tete PT_TLS).

use nxk_abi::err::{E2BIG, EFAULT};
use nxk_abi::layout::{PAGE_SIZE, STACK_SIZE};
use nxk_abi::prot;
use nxk_abi::startup::{Block, BLOCK_MAGIC, MAX_STRINGS};

use crate::kern::elf::{self, Loaded, Phdr, Source};
use crate::kern::{process, random};
use crate::mm::{uaccess, vm};

/// Origine des chaines arguments + environnement.
pub enum Strings {
    Kernel(&'static [u8]),
    /// Memoire du processus appelant (celui dont les tables sont chargees).
    User { va: u64, len: u64 },
}

impl Strings {
    fn len(&self) -> u64 {
        match self {
            Strings::Kernel(s) => s.len() as u64,
            Strings::User { len, .. } => *len,
        }
    }
}

/// Pointeur de pile et adresse du bloc pour le thread principal.
pub struct Start {
    pub sp: u64,
    pub block: u64,
}

fn bytes_of<T>(v: &T) -> &[u8] {
    unsafe { core::slice::from_raw_parts(v as *const T as *const u8, core::mem::size_of::<T>()) }
}

/// Reserve la pile principale et y ecrit le bloc de demarrage.
pub fn prepare(pi: u16, src: &dyn Source, l: &Loaded, strings: Strings, argc: u32, envc: u32) -> Result<Start, i64> {
    let slen = strings.len();
    if slen > MAX_STRINGS {
        return Err(-E2BIG);
    }
    let top = process::get(pi).stack_top;
    process::get(pi).load_base = l.base;
    process::reserve_anon(pi, top - STACK_SIZE, STACK_SIZE, prot::READ | prot::WRITE)?;

    let hdr = core::mem::size_of::<Block>() as u64;
    let phent = core::mem::size_of::<Phdr>() as u64;
    let ph_bytes = l.phnum as u64 * phent;
    let total = (hdr + ph_bytes + slen + 15) & !15;
    let block_va = (top - total) & !15;

    let mut random_seed = [0u8; 16];
    random::fill(&mut random_seed);
    let b = Block {
        magic: BLOCK_MAGIC,
        version: 1,
        argc,
        envc,
        strings: block_va + hdr + ph_bytes,
        strings_len: slen,
        phdr: block_va + hdr,
        phnum: l.phnum as u32,
        phent: phent as u32,
        load_base: l.base,
        entry: l.entry,
        stack_base: top - STACK_SIZE,
        stack_size: STACK_SIZE,
        random: random_seed,
    };
    write_mem(pi, block_va, bytes_of(&b))?;
    for k in 0..l.phnum {
        let ph = elf::phdr(src, l.phoff, k)?;
        write_mem(pi, block_va + hdr + k as u64 * phent, bytes_of(&ph))?;
    }
    let dst = b.strings;
    match strings {
        Strings::Kernel(s) => write_mem(pi, dst, s)?,
        Strings::User { va, len } => {
            let mut buf = [0u8; 256];
            let mut off = 0;
            while off < len {
                let n = (len - off).min(buf.len() as u64) as usize;
                uaccess::copy_from_user(&mut buf[..n], va + off)?;
                write_mem(pi, dst + off, &buf[..n])?;
                off += n as u64;
            }
        }
    }
    Ok(Start { sp: block_va, block: block_va })
}

/// Ecrit dans la memoire d'un processus (qui n'est pas forcement le courant),
/// en peuplant les pages reservees au passage.
pub fn write_mem(pi: u16, va: u64, bytes: &[u8]) -> Result<(), i64> {
    let l1 = process::get(pi).l1;
    let mut off = 0usize;
    while off < bytes.len() {
        let a = va + off as u64;
        let page = a & !(PAGE_SIZE - 1);
        let mut pte = vm::lookup(l1, page);
        if pte & 1 == 0 {
            if !process::populate(pi, page) {
                return Err(-EFAULT);
            }
            pte = vm::lookup(l1, page);
        }
        let n = ((PAGE_SIZE - (a & (PAGE_SIZE - 1))) as usize).min(bytes.len() - off);
        unsafe {
            core::ptr::copy_nonoverlapping(
                bytes.as_ptr().add(off),
                (vm::pte_addr(pte) + (a & (PAGE_SIZE - 1))) as *mut u8,
                n,
            )
        };
        off += n;
    }
    Ok(())
}
