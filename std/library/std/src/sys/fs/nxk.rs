//! Client of the `fs` service (FAT32 on SD): absolute UTF-8 paths, positioned I/O.
//! Relative paths resolve from `PWD`; each `File` holds its own position.

use crate::ffi::OsString;
use crate::fmt;
use crate::fs::TryLockError;
use crate::io::{self, BorrowedCursor, IoSlice, IoSliceMut, SeekFrom};
use crate::path::{Path, PathBuf};
use crate::sync::Arc;
use crate::sync::atomic::{AtomicU64, Ordering};
pub use crate::sys::fs::common::{Dir, copy, exists, remove_dir_all};
use crate::sys::pal::abi::err::{EINVAL, ENAMETOOLONG};
use crate::sys::pal::abi::proto::fs as proto;
use crate::sys::pal::ipc::{self, Msg};
use crate::sys::pal::{svc, unsupported};
use crate::sys::time::SystemTime;

/// Max path length accepted by the service.
const MAX_PATH: usize = 2047;

fn err(e: i64) -> io::Error {
    io::Error::from_raw_os_error(e as i32)
}

fn call(m: &Msg) -> io::Result<Msg> {
    svc::call(svc::fs()?, m)
}

/// Normalized absolute path (`.` and `..` resolved), UTF-8.
pub fn absolute(p: &Path) -> io::Result<String> {
    let s = p
        .as_os_str()
        .to_str()
        .ok_or(io::const_error!(io::ErrorKind::InvalidFilename, "non-UTF-8 path"))?;
    let cwd;
    let mut parts: Vec<&str> = Vec::new();
    if !s.starts_with('/') {
        cwd = crate::sys::paths::getcwd()?;
        let c = cwd.to_str().unwrap_or("/");
        parts.extend(c.split('/').filter(|c| !c.is_empty() && *c != "."));
    }
    for c in s.split('/') {
        match c {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            c => parts.push(c),
        }
    }
    let mut out = String::with_capacity(s.len() + 1);
    for c in &parts {
        out.push('/');
        out.push_str(c);
    }
    if out.is_empty() {
        out.push('/');
    }
    if out.len() > MAX_PATH {
        return Err(err(ENAMETOOLONG));
    }
    Ok(out)
}

fn path_call(label: u32, p: &Path) -> io::Result<Msg> {
    let a = absolute(p)?;
    call(&ipc::pack(label, a.as_bytes()).0)
}

// ---------------------------------------------------------------- metadata ---

#[derive(Clone)]
pub struct FileAttr {
    size: u64,
    kind: u64,
    mtime: u64,
}

impl FileAttr {
    fn from_reply(m: &Msg) -> FileAttr {
        FileAttr { size: m.mr[0], kind: m.mr[1], mtime: m.mr[2] }
    }

    pub fn size(&self) -> u64 {
        self.size
    }

    pub fn perm(&self) -> FilePermissions {
        FilePermissions { readonly: false }
    }

    pub fn file_type(&self) -> FileType {
        FileType { dir: self.kind == proto::KIND_DIR }
    }

    pub fn modified(&self) -> io::Result<SystemTime> {
        Ok(SystemTime::from_secs(self.mtime))
    }

    pub fn accessed(&self) -> io::Result<SystemTime> {
        unsupported()
    }

    pub fn created(&self) -> io::Result<SystemTime> {
        unsupported()
    }
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct FilePermissions {
    readonly: bool,
}

impl FilePermissions {
    pub fn readonly(&self) -> bool {
        self.readonly
    }

    pub fn set_readonly(&mut self, readonly: bool) {
        self.readonly = readonly
    }
}

/// FAT has no permissions: only readonly could apply, and the service
/// does not support it.
fn apply_perm(perm: &FilePermissions) -> io::Result<()> {
    if perm.readonly { unsupported() } else { Ok(()) }
}

#[derive(Copy, Clone, Debug, Default)]
pub struct FileTimes {}

impl FileTimes {
    pub fn set_accessed(&mut self, _t: SystemTime) {}
    pub fn set_modified(&mut self, _t: SystemTime) {}
}

#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug)]
pub struct FileType {
    dir: bool,
}

impl FileType {
    pub fn is_dir(&self) -> bool {
        self.dir
    }

    pub fn is_file(&self) -> bool {
        !self.dir
    }

    pub fn is_symlink(&self) -> bool {
        false
    }
}

// ------------------------------------------------------------------- dirs ---

/// Directory content, fully read at open: deleting during iteration does
/// not shift remaining entries.
pub struct ReadDir {
    dir: PathBuf,
    entries: crate::vec::IntoIter<(String, FileAttr)>,
}

pub struct DirEntry {
    dir: PathBuf,
    name: String,
    attr: FileAttr,
}

impl fmt::Debug for ReadDir {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("ReadDir").field(&self.dir).finish()
    }
}

impl Iterator for ReadDir {
    type Item = io::Result<DirEntry>;

    fn next(&mut self) -> Option<io::Result<DirEntry>> {
        let (name, attr) = self.entries.next()?;
        Some(Ok(DirEntry { dir: self.dir.clone(), name, attr }))
    }
}

impl DirEntry {
    pub fn path(&self) -> PathBuf {
        self.dir.join(&self.name)
    }

    pub fn file_name(&self) -> OsString {
        OsString::from(self.name.clone())
    }

    pub fn metadata(&self) -> io::Result<FileAttr> {
        Ok(self.attr.clone())
    }

    pub fn file_type(&self) -> io::Result<FileType> {
        Ok(self.attr.file_type())
    }
}

pub fn readdir(p: &Path) -> io::Result<ReadDir> {
    let abs = absolute(p)?;
    let mut entries = Vec::new();
    let mut name = vec![0u8; 1024];
    loop {
        let r = call(&ipc::pack_at(proto::READDIR, &[entries.len() as u64], abs.as_bytes()).0)?;
        let n = ipc::unpack_at(&r, 3, &mut name);
        if n == 0 {
            break;
        }
        let attr = FileAttr { size: r.mr[0], kind: r.mr[1], mtime: r.mr[2] };
        entries.push((String::from_utf8_lossy(&name[..n]).into_owned(), attr));
    }
    // Path as the caller gave it (as on Unix).
    Ok(ReadDir { dir: p.to_path_buf(), entries: entries.into_iter() })
}

#[derive(Debug)]
pub struct DirBuilder {}

impl DirBuilder {
    pub fn new() -> DirBuilder {
        DirBuilder {}
    }

    pub fn mkdir(&self, p: &Path) -> io::Result<()> {
        path_call(proto::MKDIR, p).map(|_| ())
    }
}

// ------------------------------------------------------------------ files ---

#[derive(Clone, Debug)]
pub struct OpenOptions {
    read: bool,
    write: bool,
    append: bool,
    truncate: bool,
    create: bool,
    create_new: bool,
}

impl OpenOptions {
    pub fn new() -> OpenOptions {
        OpenOptions { read: false, write: false, append: false, truncate: false, create: false, create_new: false }
    }

    pub fn read(&mut self, read: bool) {
        self.read = read;
    }
    pub fn write(&mut self, write: bool) {
        self.write = write;
    }
    pub fn append(&mut self, append: bool) {
        self.append = append;
    }
    pub fn truncate(&mut self, truncate: bool) {
        self.truncate = truncate;
    }
    pub fn create(&mut self, create: bool) {
        self.create = create;
    }
    pub fn create_new(&mut self, create_new: bool) {
        self.create_new = create_new;
    }

    /// Service flags, with the same rejections as Unix.
    fn flags(&self) -> io::Result<u64> {
        let writes = self.write || self.append;
        if !self.read && !writes {
            return Err(err(EINVAL));
        }
        if (self.truncate && !self.write) || ((self.create || self.create_new) && !writes) {
            return Err(err(EINVAL));
        }
        let mut f = 0;
        if self.read {
            f |= proto::O_READ;
        }
        if writes {
            f |= proto::O_WRITE;
        }
        if self.truncate && !self.append {
            f |= proto::O_TRUNC;
        }
        if self.create_new {
            f |= proto::O_CREATE | proto::O_EXCL;
        } else if self.create {
            f |= proto::O_CREATE;
        }
        Ok(f)
    }
}

pub struct File {
    fd: u64,
    /// Position shared by clones (`try_clone`), as on Unix.
    pos: Arc<AtomicU64>,
    append: bool,
    path: String,
}

impl File {
    pub fn open(p: &Path, opts: &OpenOptions) -> io::Result<File> {
        let flags = opts.flags()?;
        let abs = absolute(p)?;
        let r = call(&ipc::pack_at(proto::OPEN, &[flags], abs.as_bytes()).0)?;
        Ok(File { fd: r.mr[0], pos: Arc::new(AtomicU64::new(0)), append: opts.append, path: abs })
    }

    fn fstat(&self) -> io::Result<FileAttr> {
        call(&Msg::from_words(proto::FSTAT, &[self.fd])).map(|m| FileAttr::from_reply(&m))
    }

    pub fn file_attr(&self) -> io::Result<FileAttr> {
        self.fstat()
    }

    pub fn fsync(&self) -> io::Result<()> {
        call(&Msg::new(proto::SYNC, 0, 0)).map(|_| ())
    }

    pub fn datasync(&self) -> io::Result<()> {
        self.fsync()
    }

    pub fn lock(&self) -> io::Result<()> {
        unsupported()
    }

    pub fn lock_shared(&self) -> io::Result<()> {
        unsupported()
    }

    pub fn try_lock(&self) -> Result<(), TryLockError> {
        Err(TryLockError::Error(crate::sys::pal::unsupported_err()))
    }

    pub fn try_lock_shared(&self) -> Result<(), TryLockError> {
        Err(TryLockError::Error(crate::sys::pal::unsupported_err()))
    }

    pub fn unlock(&self) -> io::Result<()> {
        unsupported()
    }

    pub fn truncate(&self, size: u64) -> io::Result<()> {
        call(&Msg::from_words(proto::TRUNCATE, &[self.fd, size])).map(|_| ())
    }

    /// Positioned read, in `MAX_IO`-byte chunks.
    fn read_at(&self, pos: u64, buf: &mut [u8]) -> io::Result<usize> {
        let mut done = 0;
        while done < buf.len() {
            let len = (buf.len() - done).min(proto::MAX_IO);
            let r = call(&Msg::from_words(proto::READ, &[self.fd, pos + done as u64, len as u64]))?;
            let n = ipc::unpack(&r, &mut buf[done..done + len]);
            done += n;
            if n < len {
                break;
            }
        }
        Ok(done)
    }

    pub fn read(&self, buf: &mut [u8]) -> io::Result<usize> {
        let pos = self.pos.load(Ordering::Relaxed);
        let n = self.read_at(pos, buf)?;
        self.pos.store(pos + n as u64, Ordering::Relaxed);
        Ok(n)
    }

    pub fn read_vectored(&self, bufs: &mut [IoSliceMut<'_>]) -> io::Result<usize> {
        crate::io::default_read_vectored(|b| self.read(b), bufs)
    }

    pub fn is_read_vectored(&self) -> bool {
        false
    }

    pub fn read_buf(&self, cursor: BorrowedCursor<'_, u8>) -> io::Result<()> {
        crate::io::default_read_buf(|b| self.read(b), cursor)
    }

    pub fn write(&self, data: &[u8]) -> io::Result<usize> {
        let mut pos = self.pos.load(Ordering::Relaxed);
        if self.append {
            pos = self.fstat()?.size;
        }
        let mut done = 0;
        while done < data.len() {
            let end = (done + proto::MAX_IO).min(data.len());
            let head = [self.fd, pos + done as u64];
            let w = match call(&ipc::pack_at(proto::WRITE, &head, &data[done..end]).0) {
                Ok(r) => r.mr[0] as usize,
                // Partial write already done: report it, like POSIX.
                Err(_) if done > 0 => break,
                Err(e) => return Err(e),
            };
            done += w;
            if done < end {
                break;
            }
        }
        self.pos.store(pos + done as u64, Ordering::Relaxed);
        Ok(done)
    }

    pub fn write_vectored(&self, bufs: &[IoSlice<'_>]) -> io::Result<usize> {
        crate::io::default_write_vectored(|b| self.write(b), bufs)
    }

    pub fn is_write_vectored(&self) -> bool {
        false
    }

    pub fn flush(&self) -> io::Result<()> {
        Ok(())
    }

    pub fn seek(&self, pos: SeekFrom) -> io::Result<u64> {
        let base = match pos {
            SeekFrom::Start(n) => {
                self.pos.store(n, Ordering::Relaxed);
                return Ok(n);
            }
            SeekFrom::End(off) => (self.fstat()?.size, off),
            SeekFrom::Current(off) => (self.pos.load(Ordering::Relaxed), off),
        };
        let n = base.0.checked_add_signed(base.1).ok_or_else(|| err(EINVAL))?;
        self.pos.store(n, Ordering::Relaxed);
        Ok(n)
    }

    pub fn size(&self) -> Option<io::Result<u64>> {
        Some(self.fstat().map(|a| a.size))
    }

    pub fn tell(&self) -> io::Result<u64> {
        Ok(self.pos.load(Ordering::Relaxed))
    }

    pub fn duplicate(&self) -> io::Result<File> {
        call(&Msg::from_words(proto::DUP, &[self.fd]))?;
        Ok(File { fd: self.fd, pos: self.pos.clone(), append: self.append, path: self.path.clone() })
    }

    pub fn set_permissions(&self, perm: FilePermissions) -> io::Result<()> {
        apply_perm(&perm)
    }

    pub fn set_times(&self, _times: FileTimes) -> io::Result<()> {
        unsupported()
    }
}

impl Drop for File {
    fn drop(&mut self) {
        let _ = call(&Msg::from_words(proto::CLOSE, &[self.fd]));
    }
}

impl fmt::Debug for File {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("File").field("fd", &self.fd).field("path", &self.path).finish()
    }
}

// ------------------------------------------------------------------ paths ---

pub fn unlink(p: &Path) -> io::Result<()> {
    path_call(proto::UNLINK, p).map(|_| ())
}

pub fn rename(old: &Path, new: &Path) -> io::Result<()> {
    let mut s = absolute(old)?;
    s.push('\0');
    s.push_str(&absolute(new)?);
    call(&ipc::pack(proto::RENAME, s.as_bytes()).0).map(|_| ())
}

pub fn set_perm(_p: &Path, perm: FilePermissions) -> io::Result<()> {
    apply_perm(&perm)
}

pub fn set_times(_p: &Path, _times: FileTimes) -> io::Result<()> {
    unsupported()
}

pub fn set_times_nofollow(_p: &Path, _times: FileTimes) -> io::Result<()> {
    unsupported()
}

pub fn rmdir(p: &Path) -> io::Result<()> {
    path_call(proto::RMDIR, p).map(|_| ())
}

pub fn readlink(_p: &Path) -> io::Result<PathBuf> {
    unsupported()
}

pub fn symlink(_original: &Path, _link: &Path) -> io::Result<()> {
    unsupported()
}

pub fn link(_src: &Path, _dst: &Path) -> io::Result<()> {
    unsupported()
}

pub fn stat(p: &Path) -> io::Result<FileAttr> {
    path_call(proto::STAT, p).map(|m| FileAttr::from_reply(&m))
}

pub fn lstat(p: &Path) -> io::Result<FileAttr> {
    stat(p)
}

pub fn canonicalize(p: &Path) -> io::Result<PathBuf> {
    let abs = absolute(p)?;
    stat(Path::new(&abs))?;
    Ok(PathBuf::from(abs))
}
