//! Program args and environment, read from the startup block
//! the kernel drops at the top of the main stack.

use alloc::string::String;
use alloc::vec::Vec;

use nxk_abi::startup::Block;

use crate::rt;
use crate::sync::Mutex;

fn block() -> Option<&'static Block> {
    Block::at(rt::startup_block())
}

/// Block strings: the `argc` args then the `envc` variables.
fn strings() -> impl Iterator<Item = &'static str> {
    let (data, n) = match block() {
        Some(b) => (b.string_area(), (b.argc + b.envc) as usize),
        None => (&[][..], 0),
    };
    data.split(|&c| c == 0)
        .take(n)
        .map(|s| core::str::from_utf8(s).unwrap_or(""))
}

/// Program args (the first is its name).
pub fn args() -> impl Iterator<Item = &'static str> {
    let argc = block().map(|b| b.argc as usize).unwrap_or(0);
    strings().take(argc)
}

/// Startup environment, later modified via `set_var`.
static OVERRIDES: Mutex<Vec<(String, Option<String>)>> = Mutex::new(Vec::new());

fn initial_vars() -> impl Iterator<Item = (&'static str, &'static str)> {
    let argc = block().map(|b| b.argc as usize).unwrap_or(0);
    strings().skip(argc).filter_map(|kv| kv.split_once('='))
}

/// All `NAME=value` environment variables.
pub fn vars() -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> =
        initial_vars().map(|(k, v)| (k.into(), v.into())).collect();
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
    initial_vars()
        .find(|(k, _)| *k == name)
        .map(|(_, v)| v.into())
}

pub fn set_var(name: &str, value: &str) {
    OVERRIDES.lock().push((name.into(), Some(value.into())));
}

pub fn remove_var(name: &str) {
    OVERRIDES.lock().push((name.into(), None));
}

/// Raw startup block (program headers, seed, stack...).
pub fn startup_block() -> Option<&'static Block> {
    block()
}
