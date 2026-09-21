//! Lecture minimale d'un arbre de peripheriques (FDT).
//!
//! Lance par un moniteur securise ou un chargeur standard (ATF, U-Boot), nxk
//! recoit dans x0 l'adresse d'un DTB, selon le protocole de demarrage arm64.
//! Il n'en retient que la memoire : la taille de la DRAM (`/memory`) et les
//! zones a ne jamais toucher (`/memreserve/` et les noeuds de
//! `/reserved-memory`, ou vit par exemple le moniteur). Tout le reste de
//! l'arbre concerne les pilotes, en espace utilisateur.
//!
//! Lu MMU coupee, avant toute allocation : le DTB n'est jamais ecrase.

const MAGIC: u32 = 0xD00D_FEED;
const BEGIN_NODE: u32 = 1;
const END_NODE: u32 = 2;
const PROP: u32 = 3;
const NOP: u32 = 4;
const END: u32 = 9;

pub const MAX_RESERVED: usize = 8;

pub struct Info {
    /// Plage de DRAM annoncee par `/memory` (base, taille).
    pub memory: Option<(u64, u64)>,
    pub reserved: [(u64, u64); MAX_RESERVED],
    pub nreserved: usize,
    /// Le DTB lui-meme (a ne pas ecraser tant qu'il sert).
    pub blob: (u64, u64),
}

fn be32(p: u64) -> u32 {
    u32::from_be(unsafe { core::ptr::read_unaligned(p as *const u32) })
}

fn be64(p: u64) -> u64 {
    u64::from_be(unsafe { core::ptr::read_unaligned(p as *const u64) })
}

fn cstr(p: u64, max: u64) -> &'static [u8] {
    let mut n = 0;
    while n < max && unsafe { *((p + n) as *const u8) } != 0 {
        n += 1;
    }
    unsafe { core::slice::from_raw_parts(p as *const u8, n as usize) }
}

/// Lit une cellule d'adresse ou de taille (1 ou 2 mots de 32 bits).
fn cells(p: u64, n: u32) -> u64 {
    if n == 2 {
        be64(p)
    } else {
        be32(p) as u64
    }
}

/// Analyse le DTB a `addr` s'il est plausible (entierement dans `dram`).
pub fn parse(addr: u64, dram: (u64, u64)) -> Option<Info> {
    if addr == 0 || addr % 8 != 0 || addr < dram.0 || addr + 40 > dram.1 || be32(addr) != MAGIC {
        return None;
    }
    let total = be32(addr + 4) as u64;
    if total < 40 || total > 2 << 20 || addr + total > dram.1 {
        return None;
    }
    let off_struct = be32(addr + 8) as u64;
    let off_strings = be32(addr + 12) as u64;
    let off_rsv = be32(addr + 16) as u64;
    let size_struct = be32(addr + 36) as u64;
    if off_struct + size_struct > total || off_strings > total || off_rsv + 16 > total {
        return None;
    }
    let mut info = Info { memory: None, reserved: [(0, 0); MAX_RESERVED], nreserved: 0, blob: (addr, total) };
    let add = |info: &mut Info, b: u64, s: u64| {
        if s != 0 && info.nreserved < MAX_RESERVED {
            info.reserved[info.nreserved] = (b, s);
            info.nreserved += 1;
        }
    };

    // Bloc /memreserve/ : paires (adresse, taille) jusqu'a (0, 0).
    let mut p = addr + off_rsv;
    while p + 16 <= addr + total {
        let (b, s) = (be64(p), be64(p + 8));
        if b == 0 && s == 0 {
            break;
        }
        add(&mut info, b, s);
        p += 16;
    }

    // Structure : on suit la profondeur et les #address-cells/#size-cells.
    let strings = addr + off_strings;
    let end = addr + off_struct + size_struct;
    let mut p = addr + off_struct;
    let mut depth = 0usize;
    // `acells[k]`/`scells[k]` : format des `reg` des noeuds de profondeur k,
    // fixe par leur parent (defaut 2/1 selon la specification devicetree).
    let mut acells = [2u32; 9];
    let mut scells = [1u32; 9];
    let mut in_reserved = false;
    let mut reserved_depth = 0;
    let mut is_memory = false;
    while p + 4 <= end {
        let tok = be32(p);
        p += 4;
        match tok {
            BEGIN_NODE => {
                let name = cstr(p, end - p);
                p = (p + name.len() as u64 + 1 + 3) & !3;
                depth += 1;
                if depth + 1 < 9 {
                    acells[depth + 1] = 2;
                    scells[depth + 1] = 1;
                }
                is_memory = depth == 2 && (name == b"memory" || name.starts_with(b"memory@"));
                if depth == 2 && name == b"reserved-memory" {
                    in_reserved = true;
                    reserved_depth = depth;
                }
            }
            END_NODE => {
                if in_reserved && depth == reserved_depth {
                    in_reserved = false;
                }
                depth = depth.saturating_sub(1);
                is_memory = false;
            }
            PROP => {
                let len = be32(p) as u64;
                let nameoff = be32(p + 4) as u64;
                let val = p + 8;
                p = (val + len + 3) & !3;
                if nameoff >= total - off_strings {
                    return None;
                }
                let name = cstr(strings + nameoff, total - off_strings - nameoff);
                let d = depth.min(7);
                match name {
                    // Valeurs pour les enfants de ce noeud.
                    b"#address-cells" if len == 4 => acells[d + 1] = be32(val),
                    b"#size-cells" if len == 4 => scells[d + 1] = be32(val),
                    b"reg" => {
                        let (ac, sc) = (acells[d], scells[d]);
                        if !(1..=2).contains(&ac) || !(1..=2).contains(&sc) {
                            continue;
                        }
                        let stride = (ac + sc) as u64 * 4;
                        let mut q = val;
                        while q + stride <= val + len {
                            let (b, s) = (cells(q, ac), cells(q + ac as u64 * 4, sc));
                            if is_memory && info.memory.is_none() {
                                info.memory = Some((b, s));
                            } else if in_reserved && depth == reserved_depth + 1 {
                                add(&mut info, b, s);
                            }
                            q += stride;
                        }
                    }
                    _ => {}
                }
            }
            NOP => {}
            END => break,
            _ => return None,
        }
    }
    Some(info)
}
