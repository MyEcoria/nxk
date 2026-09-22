//! Program arguments, read from the startup block the kernel leaves atop
//! the main stack (the argv equivalent).

pub use super::common::Args;
use crate::ffi::OsString;
use crate::sys::FromInner;
use crate::sys::os_str::Buf;
use crate::sys::pal::startup_strings;

pub fn args() -> Args {
    let (data, argc, _) = startup_strings();
    let args = data
        .split(|&c| c == 0)
        .take(argc)
        .map(|s| OsString::from_inner(Buf { inner: s.to_vec() }))
        .collect();
    Args::new(args)
}
