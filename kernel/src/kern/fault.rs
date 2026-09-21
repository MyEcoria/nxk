//! Fautes des programmes utilisateur : le processus fautif est termine, le
//! reste du systeme continue.

use nxk_abi::debug;

use crate::kern::{process, thread};
use crate::kprintln;

/// Code de sortie d'un processus tue par une faute (comme SIGSEGV sous Linux).
pub const EXIT_FAULT: i64 = 139;

fn describe(ec: u64) -> &'static str {
    match ec {
        0x00 => "instruction indefinie",
        0x0E => "etat d'execution illegal",
        0x18 => "acces a un registre systeme interdit",
        0x20 => "faute d'instruction",
        0x22 => "PC non aligne",
        0x24 => "faute de donnees",
        0x26 => "SP non aligne",
        0x2F => "erreur systeme (SError)",
        0x32 => "pas a pas",
        0x3C => "point d'arret (BRK)",
        _ => "exception inattendue",
    }
}

pub fn user_fault(ti: u16, esr: u64, far: u64) {
    let t = thread::get(ti);
    let pi = t.proc_idx;
    let ec = esr >> 26;
    // Faute de traduction (niveaux 0-3) sur une page reservee : on la peuple
    // et l'instruction est reexecutee au retour.
    if matches!(ec, 0x20 | 0x24) && (0x04..=0x07).contains(&(esr & 0x3F)) && process::populate(pi, far) {
        return;
    }
    // Processus debogue : l'evenement part au debogueur au lieu de le tuer.
    let kind = match ec {
        0x3C => debug::KIND_BREAK,
        0x32 => {
            t.single_step = false;
            t.frame.spsr &= !crate::kern::debug::SPSR_SS;
            debug::KIND_STEP
        }
        _ => debug::KIND_FAULT,
    };
    if crate::kern::debug::event(ti, kind, esr, far) {
        return;
    }
    if ec == 0x32 {
        return; // pas a pas termine alors que le debogueur est parti
    }
    let base = process::get(pi).load_base;
    let off = t.frame.elr.wrapping_sub(base);
    // Faute de donnees : ecriture ou lecture, et nature (traduction : rien
    // n'est projete ; permission : page protegee).
    let detail = if ec == 0x24 {
        let dfsc = esr & 0x3F;
        let what = match dfsc {
            0x04..=0x07 => "page absente",
            0x09..=0x0B => "drapeau d'acces",
            0x0D..=0x0F => "permission",
            0x21 => "alignement",
            _ => "autre",
        };
        if esr & (1 << 6) != 0 { (" (ecriture, ", what) } else { (" (lecture, ", what) }
    } else {
        ("", "")
    };
    kprintln!(
        "[nxk] '{}' (pid {}, thread {}) : {} a {:#x}{}{}{}, pc {:#x} (+{:#x}), sp {:#x}, lr +{:#x} -> processus termine",
        process::name(pi),
        process::get(pi).pid,
        t.tid,
        describe(ec),
        far,
        detail.0,
        detail.1,
        if detail.0.is_empty() { "" } else { ")" },
        t.frame.elr,
        off,
        t.frame.sp,
        t.frame.x[30].wrapping_sub(base)
    );
    process::kill(pi, EXIT_FAULT);
}
