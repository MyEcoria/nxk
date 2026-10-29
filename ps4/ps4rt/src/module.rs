//! Loading a module: its segments into guest memory, its symbols and relocations read from
//! either dynamic layout.
//!
//! A PS4 image keeps its tables in the `PT_SCE_DYNLIBDATA` segment under the SCE tags, with
//! symbol names written `NID#library#module`; a PC-built shared object keeps them in loaded
//! memory under the standard `DT_*` tags with plain names. Both fill the same `Module`.

use ps4fmt::elf::{Image, PT_DYNAMIC, PT_LOAD, PT_SCE_DYNLIBDATA};
use ps4fmt::{nid::nid, span, u32le, u64le};

use crate::mem::GuestMem;
use crate::Error;

/// PS4 RELRO segment, loaded like PT_LOAD.
const PT_SCE_RELRO: u32 = 0x6100_0010;

mod dt {
    pub const NULL: u64 = 0;
    pub const NEEDED: u64 = 1;
    pub const PLTRELSZ: u64 = 2;
    pub const HASH: u64 = 4;
    pub const STRTAB: u64 = 5;
    pub const SYMTAB: u64 = 6;
    pub const RELA: u64 = 7;
    pub const RELASZ: u64 = 8;
    pub const STRSZ: u64 = 10;
    pub const JMPREL: u64 = 23;
    // SCE tags, as upstream's port/scripts/prepare.py reads them.
    pub const SCE_NEEDED_MODULE: u64 = 0x6100_000f;
    pub const SCE_IMPORT_LIB: u64 = 0x6100_0015;
    pub const SCE_JMPREL: u64 = 0x6100_0029;
    pub const SCE_PLTRELSZ: u64 = 0x6100_002d;
    pub const SCE_RELA: u64 = 0x6100_002f;
    pub const SCE_RELASZ: u64 = 0x6100_0031;
    pub const SCE_STRTAB: u64 = 0x6100_0035;
    pub const SCE_STRSZ: u64 = 0x6100_0037;
    pub const SCE_SYMTAB: u64 = 0x6100_0039;
    pub const SCE_SYMTABSZ: u64 = 0x6100_003f;
}

const SHN_ABS: u16 = 0xfff1;

/// A symbol of the module's table.
#[derive(Debug, Clone)]
pub struct Symbol {
    /// The 11-character NID (plain names are hashed to theirs).
    pub nid: String,
    /// Library an import comes from, when the image names it (SCE layout).
    pub lib: Option<String>,
    /// Section index; 0 for an import.
    pub shndx: u16,
    /// STT_* type and STB_* binding.
    pub kind: u8,
    pub bind: u8,
    pub value: u64,
}

impl Symbol {
    pub fn is_import(&self) -> bool {
        self.shndx == 0
    }
}

/// One `Elf64_Rela`.
#[derive(Debug, Clone, Copy)]
pub struct Reloc {
    pub offset: u64,
    pub kind: u32,
    pub sym: u32,
    pub addend: i64,
}

/// A module mapped into guest memory, not yet linked.
#[derive(Debug)]
pub struct Module {
    pub name: String,
    /// Library its exports belong to (what an SCE import names), e.g. `libc`.
    pub lib: String,
    pub base: u64,
    pub size: u64,
    pub entry: u64,
    pub symbols: Vec<Symbol>,
    pub relocs: Vec<Reloc>,
    /// Modules it needs, by name.
    pub needed: Vec<String>,
}

impl Module {
    /// Address of an exported symbol with this NID.
    pub fn export(&self, nid: &str) -> Option<u64> {
        self.symbols
            .iter()
            .find(|s| !s.is_import() && matches!(s.bind, 1 | 2) && s.nid == nid)
            .map(|s| self.symbol_addr(s))
    }

    /// Address of a defined symbol.
    pub fn symbol_addr(&self, s: &Symbol) -> u64 {
        if s.shndx == SHN_ABS {
            s.value
        } else {
            self.base.wrapping_add(s.value)
        }
    }
}

/// Where the dynamic tables live and what they say.
struct Tables<'a> {
    strtab: &'a [u8],
    symtab: &'a [u8],
    rela: &'a [u8],
    jmprel: &'a [u8],
    /// SCE library ids -> names.
    libs: Vec<(u64, String)>,
    needed: Vec<String>,
    sce: bool,
}

fn cstr(tab: &[u8], off: u64) -> Result<String, Error> {
    let s = tab
        .get(off as usize..)
        .ok_or(Error::Dynamic("string offset"))?;
    let end = s
        .iter()
        .position(|&b| b == 0)
        .ok_or(Error::Dynamic("unterminated string"))?;
    core::str::from_utf8(&s[..end])
        .map(str::to_owned)
        .map_err(|_| Error::Dynamic("non-UTF-8 name"))
}

/// File offset of a virtual address inside a file-backed PT_LOAD.
fn file_off(img: &Image, va: u64) -> Result<u64, Error> {
    img.phdrs
        .iter()
        .find(|p| p.kind == PT_LOAD && va >= p.vaddr && va - p.vaddr < p.filesz)
        .map(|p| p.offset + va - p.vaddr)
        .ok_or(Error::Dynamic("table address outside the file"))
}

fn tables(img: &Image) -> Result<Tables<'_>, Error> {
    let elf = &img.elf[..];
    let dyn_ph = img
        .phdrs
        .iter()
        .find(|p| p.kind == PT_DYNAMIC)
        .ok_or(Error::Dynamic("no PT_DYNAMIC"))?;
    let mut tags = Vec::new();
    for at in (dyn_ph.offset..dyn_ph.offset + dyn_ph.filesz).step_by(16) {
        let (t, v) = (u64le(elf, at)?, u64le(elf, at + 8)?);
        if t == dt::NULL {
            break;
        }
        tags.push((t, v));
    }
    let get = |t| tags.iter().find(|(k, _)| *k == t).map(|(_, v)| *v);
    let sce = get(dt::SCE_STRTAB).is_some();
    if sce {
        let lib = img
            .phdrs
            .iter()
            .find(|p| p.kind == PT_SCE_DYNLIBDATA)
            .ok_or(Error::Dynamic("no PT_SCE_DYNLIBDATA"))?;
        let blob = span(elf, lib.offset, lib.filesz)?;
        let part = |off: u64, size: u64| span(blob, off, size).map_err(Error::from);
        let table = |o, s| match (get(o), get(s)) {
            (Some(o), Some(s)) => part(o, s),
            _ => Ok(&[][..]),
        };
        let strtab = table(dt::SCE_STRTAB, dt::SCE_STRSZ)?;
        let mut libs = Vec::new();
        let mut needed = Vec::new();
        for &(t, v) in &tags {
            if t == dt::SCE_IMPORT_LIB {
                libs.push((v >> 48, cstr(strtab, v & 0xffff_ffff)?));
            } else if t == dt::SCE_NEEDED_MODULE {
                needed.push(cstr(strtab, v & 0xffff_ffff)?);
            }
        }
        return Ok(Tables {
            strtab,
            symtab: table(dt::SCE_SYMTAB, dt::SCE_SYMTABSZ)?,
            rela: table(dt::SCE_RELA, dt::SCE_RELASZ)?,
            jmprel: table(dt::SCE_JMPREL, dt::SCE_PLTRELSZ)?,
            libs,
            needed,
            sce,
        });
    }
    let at =
        |va: u64, size: u64| -> Result<&[u8], Error> { Ok(span(elf, file_off(img, va)?, size)?) };
    let need = |t| get(t).ok_or(Error::Dynamic("missing dynamic tag"));
    let strtab = at(need(dt::STRTAB)?, need(dt::STRSZ)?)?;
    // DT_HASH's nchain is the symbol count (the guests are linked with --hash-style=sysv).
    let nsyms = u32le(elf, file_off(img, need(dt::HASH)?)? + 4)? as u64;
    let opt = |o, s| match (get(o), get(s)) {
        (Some(o), Some(s)) if s > 0 => at(o, s),
        _ => Ok(&[][..]),
    };
    let mut needed = Vec::new();
    for &(t, v) in &tags {
        if t == dt::NEEDED {
            needed.push(cstr(strtab, v)?);
        }
    }
    Ok(Tables {
        strtab,
        symtab: at(need(dt::SYMTAB)?, nsyms * 24)?,
        rela: opt(dt::RELA, dt::RELASZ)?,
        jmprel: opt(dt::JMPREL, dt::PLTRELSZ)?,
        libs: Vec::new(),
        needed,
        sce,
    })
}

/// Library id as written in SCE symbol names (base-64 digits).
fn dec_id(s: &str) -> Option<u64> {
    const B64: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+-";
    s.bytes().try_fold(0u64, |n, c| {
        let d = B64.iter().position(|&x| x == c)? as u64;
        n.checked_mul(64)?.checked_add(d)
    })
}

fn symbols(t: &Tables) -> Result<Vec<Symbol>, Error> {
    if !t.symtab.len().is_multiple_of(24) {
        return Err(Error::Dynamic("symbol table size"));
    }
    let mut out = Vec::with_capacity(t.symtab.len() / 24);
    for e in t.symtab.as_chunks::<24>().0 {
        let name_off = u32le(e, 0)? as u64;
        let (info, shndx) = (e[4], u16::from_le_bytes([e[6], e[7]]));
        let name = if name_off == 0 {
            String::new()
        } else {
            cstr(t.strtab, name_off)?
        };
        let (nid_part, lib) = if t.sce {
            let mut parts = name.split('#');
            let n = parts.next().unwrap_or("").to_owned();
            let lib = parts
                .next()
                .and_then(dec_id)
                .and_then(|id| t.libs.iter().find(|(i, _)| *i == id))
                .map(|(_, l)| l.clone());
            (n, lib)
        } else if name.is_empty() {
            (String::new(), None)
        } else {
            (nid(&name), None)
        };
        out.push(Symbol {
            nid: nid_part,
            lib,
            shndx,
            kind: info & 15,
            bind: info >> 4,
            value: u64le(e, 8)?,
        });
    }
    Ok(out)
}

fn relocs(tab: &[u8]) -> Result<Vec<Reloc>, Error> {
    if !tab.len().is_multiple_of(24) {
        return Err(Error::Dynamic("relocation table size"));
    }
    tab.as_chunks::<24>()
        .0
        .iter()
        .map(|e| {
            let info = u64le(e, 8)?;
            Ok(Reloc {
                offset: u64le(e, 0)?,
                kind: info as u32,
                sym: (info >> 32) as u32,
                addend: u64le(e, 16)? as i64,
            })
        })
        .collect()
}

/// Map `img`'s loadable segments at `base` and read its dynamic tables.
///
/// # Args
/// * `name` - module name (diagnostics)
/// * `lib` - library its exports serve, e.g. `libc`
/// * `img` - the parsed image
/// * `base` - guest address of its virtual address 0 (page aligned)
/// * `mem` - guest memory the module is mapped into
///
/// # Returns
/// The module, its relocations not yet applied.
///
/// # Raises
/// `Error::Map` when the range is taken, `Error::Dynamic` / `Error::Format` for a bad image.
///
/// # Example
/// ```no_run
/// # let file: &[u8] = &[];
/// let img = ps4fmt::elf::parse(file).unwrap();
/// let mut mem = ps4rt::mem::GuestMem::default();
/// let m = ps4rt::module::load("libc.prx", "libc", &img, 0x1_0000_0000, &mut mem).unwrap();
/// ```
pub fn load(
    name: &str,
    lib: &str,
    img: &Image,
    base: u64,
    mem: &mut GuestMem,
) -> Result<Module, Error> {
    let loads: Vec<_> = img
        .phdrs
        .iter()
        .filter(|p| matches!(p.kind, PT_LOAD | PT_SCE_RELRO) && p.memsz > 0)
        .collect();
    let end = loads
        .iter()
        .map(|p| p.vaddr.checked_add(p.memsz))
        .try_fold(0u64, |m, e| e.map(|e| m.max(e)))
        .ok_or(Error::Dynamic("segment wraps"))?;
    if loads.is_empty() || end > ps4fmt::elf::IMAGE_LIMIT {
        return Err(Error::Dynamic("no loadable segment or image too large"));
    }
    let size = (end + 0xfff) & !0xfff;
    if !mem.map(base, size) {
        return Err(Error::Map(base));
    }
    for p in &loads {
        if p.filesz > p.memsz {
            return Err(Error::Dynamic("segment file size above memory size"));
        }
        let bytes = span(&img.elf, p.offset, p.filesz)?;
        let dst = mem
            .slice(base + p.vaddr, bytes.len())
            .ok_or(Error::Map(base + p.vaddr))?;
        dst.copy_from_slice(bytes);
    }
    let t = tables(img)?;
    let mut all = relocs(t.rela)?;
    all.extend(relocs(t.jmprel)?);
    Ok(Module {
        name: name.to_owned(),
        lib: lib.to_owned(),
        base,
        size,
        entry: base.wrapping_add(img.entry),
        symbols: symbols(&t)?,
        relocs: all,
        needed: t.needed,
    })
}
