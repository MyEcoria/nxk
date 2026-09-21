//! Chemins du processus. Le dossier courant est la variable `PWD` (convention
//! du shell nxk, transmise aux programmes lances) ; « / » par defaut.

use crate::ffi::{OsStr, OsString};
use crate::path::{self, Path, PathBuf};
use crate::sys::FromInner;
use crate::sys::os_str::Buf;
use crate::{fmt, io};

pub fn getcwd() -> io::Result<PathBuf> {
    match crate::sys::env::getenv(OsStr::new("PWD")) {
        Some(p) if p.as_encoded_bytes().starts_with(b"/") => Ok(PathBuf::from(p)),
        _ => Ok(PathBuf::from("/")),
    }
}

pub fn chdir(p: &path::Path) -> io::Result<()> {
    let abs = crate::sys::fs::nxk_absolute(p)?;
    if !crate::sys::fs::metadata(Path::new(OsStr::new(&abs)))?.file_type().is_dir() {
        return Err(io::Error::from_raw_os_error(crate::sys::pal::abi::err::ENOTDIR as i32));
    }
    unsafe { crate::sys::env::setenv(OsStr::new("PWD"), OsStr::new(&abs)) }
}

/// Liste « a:b:c » (comme `PATH` sous Unix).
pub struct SplitPaths<'a> {
    rest: Option<&'a [u8]>,
}

pub fn split_paths(unparsed: &OsStr) -> SplitPaths<'_> {
    SplitPaths { rest: Some(unparsed.as_encoded_bytes()) }
}

impl<'a> Iterator for SplitPaths<'a> {
    type Item = PathBuf;
    fn next(&mut self) -> Option<PathBuf> {
        let s = self.rest?;
        let (item, rest) = match s.iter().position(|&c| c == b':') {
            Some(i) => (&s[..i], Some(&s[i + 1..])),
            None => (s, None),
        };
        self.rest = rest;
        Some(PathBuf::from(OsString::from_inner(Buf { inner: item.to_vec() })))
    }
}

#[derive(Debug)]
pub struct JoinPathsError;

pub fn join_paths<I, T>(paths: I) -> Result<OsString, JoinPathsError>
where
    I: Iterator<Item = T>,
    T: AsRef<OsStr>,
{
    let mut out = Vec::new();
    for (i, p) in paths.enumerate() {
        let p = p.as_ref().as_encoded_bytes();
        if p.contains(&b':') {
            return Err(JoinPathsError);
        }
        if i > 0 {
            out.push(b':');
        }
        out.extend_from_slice(p);
    }
    Ok(OsString::from_inner(Buf { inner: out }))
}

impl fmt::Display for JoinPathsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        "path segment contains separator `:`".fmt(f)
    }
}

impl crate::error::Error for JoinPathsError {}

pub fn current_exe() -> io::Result<PathBuf> {
    // Le noyau ne garde pas le chemin de l'executable ; le premier argument
    // est ce que le lanceur a demande (nom de l'image de demarrage ou chemin).
    match crate::env::args_os().next() {
        Some(a) if a.as_encoded_bytes().starts_with(b"/") => Ok(PathBuf::from(a)),
        Some(a) => Ok(PathBuf::from(format!("/bin/{}", a.to_string_lossy()))),
        None => crate::sys::pal::unsupported(),
    }
}

pub fn temp_dir() -> PathBuf {
    crate::sys::env::getenv(OsStr::new("TMPDIR")).map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/tmp"))
}

pub fn home_dir() -> Option<PathBuf> {
    crate::sys::env::getenv(OsStr::new("HOME")).map(PathBuf::from)
}
