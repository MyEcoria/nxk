//! Arguments et environnement du programme, lus dans le bloc de demarrage
//! que le noyau depose en haut de la pile principale.

use alloc::string::String;
use alloc::vec::Vec;

use nxk_abi::startup::{Block, BLOCK_MAGIC};

use crate::rt;
use crate::sync::Mutex;

fn block() -> Option<&'static Block> {
    let p = rt::startup_block();
    if p == 0 {
        return None;
    }
    let b = unsafe { &*(p as *const Block) };
    (b.magic == BLOCK_MAGIC).then_some(b)
}

/// Chaines du bloc : les `argc` arguments puis les `envc` variables.
fn strings() -> impl Iterator<Item = &'static str> {
    let (data, n) = match block() {
        Some(b) => (
            unsafe { core::slice::from_raw_parts(b.strings as *const u8, b.strings_len as usize) },
            (b.argc + b.envc) as usize,
        ),
        None => (&[][..], 0),
    };
    data.split(|&c| c == 0).take(n).map(|s| core::str::from_utf8(s).unwrap_or(""))
}

/// Arguments du programme (le premier est son nom).
pub fn args() -> impl Iterator<Item = &'static str> {
    let argc = block().map(|b| b.argc as usize).unwrap_or(0);
    strings().take(argc)
}

/// Environnement recu au demarrage, modifie ensuite par `set_var`.
static OVERRIDES: Mutex<Vec<(String, Option<String>)>> = Mutex::new(Vec::new());

fn initial_vars() -> impl Iterator<Item = (&'static str, &'static str)> {
    let argc = block().map(|b| b.argc as usize).unwrap_or(0);
    strings().skip(argc).filter_map(|kv| kv.split_once('='))
}

/// Toutes les variables d'environnement « NOM=valeur ».
pub fn vars() -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = initial_vars().map(|(k, v)| (k.into(), v.into())).collect();
    for (k, v) in OVERRIDES.lock().iter() {
        out.retain(|(x, _)| x != k);
        if let Some(v) = v {
            out.push((k.clone(), v.clone()));
        }
    }
    out
}

pub fn var(name: &str) -> Option<String> {
    for (k, v) in OVERRIDES.lock().iter().rev() {
        if k == name {
            return v.clone();
        }
    }
    initial_vars().find(|(k, _)| *k == name).map(|(_, v)| v.into())
}

pub fn set_var(name: &str, value: &str) {
    OVERRIDES.lock().push((name.into(), Some(value.into())));
}

pub fn remove_var(name: &str) {
    OVERRIDES.lock().push((name.into(), None));
}

/// Bloc de demarrage brut (en-tetes de programme, graine, pile...).
pub fn startup_block() -> Option<&'static Block> {
    block()
}
