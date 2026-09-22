//! File client of the "fs" service (paths from `PWD`).
//! Position lives client-side; the service does positioned I/O.

use alloc::string::String;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU32, Ordering};

use nxk_abi::err::{EIO, ENOSYS};
use nxk_abi::proto::fs as proto;

use crate::ipc::{self, Msg};
use crate::{env, ns};

pub use proto::{O_CREATE, O_EXCL, O_READ, O_TRUNC, O_WRITE};

static FS: AtomicU32 = AtomicU32::new(0);

/// Set the file service to use (for init, which cannot look
/// itself up in the name service).
pub fn set_handle(h: u32) {
    FS.store(h, Ordering::Relaxed);
}

fn fs() -> Result<u32, i64> {
    let h = FS.load(Ordering::Relaxed);
    if h != 0 {
        return Ok(h);
    }
    let h = ns::wait_for_timeout("fs", 2_000_000_000).map_err(|_| -ENOSYS)?;
    FS.store(h, Ordering::Relaxed);
    Ok(h)
}

fn call(m: &Msg) -> Result<Msg, i64> {
    ipc::call_checked(fs()?, m)
}

/// Normalized absolute path ("." and ".." resolved).
pub fn absolute(path: &str) -> String {
    let mut parts: Vec<&str> = Vec::new();
    let cwd = env::var("PWD").unwrap_or_else(|| String::from("/"));
    let full = if path.starts_with('/') {
        String::from(path)
    } else {
        alloc::format!("{}/{}", cwd, path)
    };
    for c in full.split('/') {
        match c {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            c => parts.push(c),
        }
    }
    let mut s = String::from("/");
    s.push_str(&parts.join("/"));
    s
}

fn path_call(label: u32, path: &str) -> Result<Msg, i64> {
    call(&ipc::pack(label, absolute(path).as_bytes()))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    File,
    Dir,
}

fn kind(v: u64) -> Kind {
    if v == proto::KIND_DIR {
        Kind::Dir
    } else {
        Kind::File
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Stat {
    pub size: u64,
    pub kind: Kind,
    /// Seconds since 1970.
    pub mtime: u64,
}

pub struct File {
    fd: u64,
    pos: u64,
    size: u64,
}

impl File {
    pub fn open(path: &str, flags: u64) -> Result<Self, i64> {
        let m = call(&ipc::pack_at(
            proto::OPEN,
            &[flags],
            absolute(path).as_bytes(),
        ))?;
        Ok(Self {
            fd: m.mr[0],
            pos: 0,
            size: m.mr[1],
        })
    }

    /// Create (or truncate) a file for writing.
    pub fn create(path: &str) -> Result<Self, i64> {
        Self::open(path, O_READ | O_WRITE | O_CREATE | O_TRUNC)
    }

    pub fn size(&self) -> u64 {
        self.size
    }

    pub fn seek(&mut self, pos: u64) {
        self.pos = pos;
    }

    pub fn position(&self) -> u64 {
        self.pos
    }

    pub fn read_at(&self, pos: u64, buf: &mut [u8]) -> Result<usize, i64> {
        let mut done = 0;
        while done < buf.len() {
            let len = (buf.len() - done).min(proto::MAX_IO);
            let mut m = Msg::new(proto::READ, 3, 0);
            m.mr[0] = self.fd;
            m.mr[1] = pos + done as u64;
            m.mr[2] = len as u64;
            let r = call(&m)?;
            let n = ipc::unpack(&r, &mut buf[done..done + len]);
            done += n;
            if n < len {
                break;
            }
        }
        Ok(done)
    }

    pub fn read(&mut self, buf: &mut [u8]) -> Result<usize, i64> {
        let n = self.read_at(self.pos, buf)?;
        self.pos += n as u64;
        Ok(n)
    }

    pub fn write_at(&mut self, pos: u64, data: &[u8]) -> Result<usize, i64> {
        let mut done = 0;
        while done < data.len() {
            let chunk = &data[done..(done + proto::MAX_IO).min(data.len())];
            let r = call(&ipc::pack_at(
                proto::WRITE,
                &[self.fd, pos + done as u64],
                chunk,
            ))?;
            if r.mr[0] as usize != chunk.len() {
                return Err(-EIO);
            }
            done += chunk.len();
        }
        self.size = self.size.max(pos + data.len() as u64);
        Ok(done)
    }

    pub fn write(&mut self, data: &[u8]) -> Result<usize, i64> {
        let n = self.write_at(self.pos, data)?;
        self.pos += n as u64;
        Ok(n)
    }

    pub fn set_len(&mut self, size: u64) -> Result<(), i64> {
        let mut m = Msg::new(proto::TRUNCATE, 2, 0);
        m.mr[0] = self.fd;
        m.mr[1] = size;
        call(&m)?;
        self.size = size;
        Ok(())
    }
}

impl Drop for File {
    fn drop(&mut self) {
        let mut m = Msg::new(proto::CLOSE, 1, 0);
        m.mr[0] = self.fd;
        let _ = call(&m);
    }
}

pub fn stat(path: &str) -> Result<Stat, i64> {
    let m = path_call(proto::STAT, path)?;
    Ok(Stat {
        size: m.mr[0],
        kind: kind(m.mr[1]),
        mtime: m.mr[2],
    })
}

#[derive(Clone, Debug)]
pub struct DirEntry {
    pub name: String,
    pub stat: Stat,
}

pub fn read_dir(path: &str) -> Result<Vec<DirEntry>, i64> {
    let p = absolute(path);
    let mut out = Vec::new();
    let mut name = [0u8; 1024];
    loop {
        let m = call(&ipc::pack_at(
            proto::READDIR,
            &[out.len() as u64],
            p.as_bytes(),
        ))?;
        let n = ipc::unpack_at(&m, 3, &mut name);
        if n == 0 {
            return Ok(out);
        }
        out.push(DirEntry {
            name: String::from(core::str::from_utf8(&name[..n]).unwrap_or("?")),
            stat: Stat {
                size: m.mr[0],
                kind: kind(m.mr[1]),
                mtime: m.mr[2],
            },
        });
    }
}

pub fn read(path: &str) -> Result<Vec<u8>, i64> {
    let f = File::open(path, O_READ)?;
    let mut v = alloc::vec![0u8; f.size() as usize];
    let n = f.read_at(0, &mut v)?;
    v.truncate(n);
    Ok(v)
}

pub fn write(path: &str, data: &[u8]) -> Result<(), i64> {
    File::create(path)?.write(data).map(|_| ())
}

pub fn mkdir(path: &str) -> Result<(), i64> {
    path_call(proto::MKDIR, path).map(|_| ())
}

pub fn remove_file(path: &str) -> Result<(), i64> {
    path_call(proto::UNLINK, path).map(|_| ())
}

pub fn remove_dir(path: &str) -> Result<(), i64> {
    path_call(proto::RMDIR, path).map(|_| ())
}

pub fn rename(from: &str, to: &str) -> Result<(), i64> {
    let s = alloc::format!("{}\0{}", absolute(from), absolute(to));
    call(&ipc::pack(proto::RENAME, s.as_bytes())).map(|_| ())
}

pub fn sync() -> Result<(), i64> {
    call(&Msg::new(proto::SYNC, 0, 0)).map(|_| ())
}

/// (free bytes, total bytes).
pub fn statfs() -> Result<(u64, u64), i64> {
    let m = call(&Msg::new(proto::STATFS, 0, 0))?;
    Ok((m.mr[0], m.mr[1]))
}
