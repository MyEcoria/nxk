//! `NAME=value` env from the startup block (copy-on-first-use); current dir is `PWD`.

pub use super::common::Env;
use crate::ffi::{OsStr, OsString};
use crate::io;
use crate::sync::Mutex;
use crate::sys::FromInner;
use crate::sys::os_str::Buf;
use crate::sys::pal::startup_strings;

type Vars = Vec<(Vec<u8>, Vec<u8>)>;

static VARS: Mutex<Option<Vars>> = Mutex::new(None);

fn initial() -> Vars {
    let (data, argc, envc) = startup_strings();
    data.split(|&c| c == 0)
        .skip(argc)
        .take(envc)
        .filter_map(|kv| {
            let eq = kv.iter().position(|&c| c == b'=')?;
            Some((kv[..eq].to_vec(), kv[eq + 1..].to_vec()))
        })
        .collect()
}

fn with<R>(f: impl FnOnce(&mut Vars) -> R) -> R {
    let mut g = VARS.lock().unwrap_or_else(|e| e.into_inner());
    f(g.get_or_insert_with(initial))
}

fn os(b: &[u8]) -> OsString {
    OsString::from_inner(Buf { inner: b.to_vec() })
}

pub fn env() -> Env {
    with(|v| Env::new(v.iter().map(|(k, x)| (os(k), os(x))).collect()))
}

pub fn getenv(key: &OsStr) -> Option<OsString> {
    let key = key.as_encoded_bytes();
    with(|v| v.iter().find(|(k, _)| k == key).map(|(_, x)| os(x)))
}

pub unsafe fn setenv(key: &OsStr, val: &OsStr) -> io::Result<()> {
    let (key, val) = (key.as_encoded_bytes(), val.as_encoded_bytes());
    if key.is_empty() || key.contains(&b'=') || key.contains(&0) || val.contains(&0) {
        return Err(io::const_error!(io::ErrorKind::InvalidInput, "invalid environment variable"));
    }
    with(|v| match v.iter_mut().find(|(k, _)| k == key) {
        Some((_, x)) => *x = val.to_vec(),
        None => v.push((key.to_vec(), val.to_vec())),
    });
    Ok(())
}

pub unsafe fn unsetenv(key: &OsStr) -> io::Result<()> {
    let key = key.as_encoded_bytes();
    with(|v| v.retain(|(k, _)| k != key));
    Ok(())
}

/// `NAME=value` environment (for spawning a program).
pub fn pairs() -> Vec<Vec<u8>> {
    with(|v| {
        v.iter()
            .map(|(k, x)| {
                let mut s = k.clone();
                s.push(b'=');
                s.extend_from_slice(x);
                s
            })
            .collect()
    })
}
