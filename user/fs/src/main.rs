//! fs: SD card FAT32 filesystem over "blk:sd" (`fs` protocol).
//! Write-back sector cache; flushed on file close or `sync`.

#![no_std]
#![no_main]

extern crate alloc;

mod cache;
mod clock;
mod fat;

use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

use libnx::abi::err::{EBADF, EEXIST, EINVAL, EISDIR, ENOENT, ENOTDIR};
use libnx::abi::proto::{blk, fs as proto};
use libnx::abi::shm::{CONTIGUOUS, DMA32, UNCACHED};
use libnx::drv;
use libnx::ipc::{self, Msg};
use libnx::mem::SharedMemory;
use libnx::{ns, println};

use cache::{Cache, IO_SECTORS, SECTOR};
use fat::{Cursor, Entry, Volume};

struct Open {
    entry: Entry,
    cur: Cursor,
    flags: u64,
    /// References (DUP): CLOSE only closes on the last one.
    refs: u32,
    /// Owner process (pid stamped by the kernel on each message): only it
    /// may use the descriptor, and its files are closed if it dies first.
    owner: u32,
}

struct Server {
    vol: Volume,
    files: Vec<Option<Open>>,
}

fn kind(e: &Entry) -> u64 {
    if e.is_dir() {
        proto::KIND_DIR
    } else {
        proto::KIND_FILE
    }
}

impl Server {
    fn is_open(files: &[Option<Open>], e: &Entry) -> bool {
        files
            .iter()
            .flatten()
            .any(|o| o.entry.dir == e.dir && o.entry.index == e.index)
    }

    fn fd(&mut self, fd: u64, pid: u32) -> Result<&mut Open, i64> {
        self.files
            .get_mut((fd as usize).wrapping_sub(1))
            .and_then(|o| o.as_mut())
            .filter(|o| o.owner == pid)
            .ok_or(-EBADF)
    }

    /// Close files of dead processes; true if there were any.
    fn reap(&mut self) -> bool {
        let live = libnx::process::live_pids();
        let mut closed = false;
        for f in self.files.iter_mut() {
            if f.as_ref().is_some_and(|o| !live.contains(&o.owner)) {
                *f = None;
                closed = true;
            }
        }
        if closed {
            let _ = self.vol.sync();
        }
        closed
    }

    /// Operation refused on an open file (EBUSY): maybe held by a dead
    /// process; reap and retry once.
    fn retry_busy<T>(&mut self, mut op: impl FnMut(&mut Self) -> Result<T, i64>) -> Result<T, i64> {
        match op(self) {
            Err(e) if e == -libnx::abi::err::EBUSY && self.reap() => op(self),
            r => r,
        }
    }

    fn open(&mut self, path: &str, flags: u64, owner: u32) -> Result<(u64, Entry), i64> {
        if self.files.iter().flatten().count() >= 32 {
            self.reap();
        }
        let e = match self.vol.lookup(path) {
            Ok(e) => {
                if flags & proto::O_CREATE != 0 && flags & proto::O_EXCL != 0 {
                    return Err(-EEXIST);
                }
                if e.is_dir() && flags & proto::O_WRITE != 0 {
                    return Err(-EISDIR);
                }
                e
            }
            Err(err) if err == -ENOENT && flags & proto::O_CREATE != 0 => {
                let (parent, name) = self.vol.parent(path)?;
                self.vol
                    .create(&parent, name, Volume::default_attr(), 0, 0)?
            }
            Err(err) => return Err(err),
        };
        let mut o = Open {
            entry: e,
            cur: Cursor::default(),
            flags,
            refs: 1,
            owner,
        };
        if flags & proto::O_TRUNC != 0 && flags & proto::O_WRITE != 0 && o.entry.size != 0 {
            self.vol.truncate(&mut o.entry, &mut o.cur, 0)?;
        }
        let e = o.entry.clone();
        let slot = match self.files.iter().position(|f| f.is_none()) {
            Some(k) => k,
            None => {
                self.files.push(None);
                self.files.len() - 1
            }
        };
        self.files[slot] = Some(o);
        Ok((slot as u64 + 1, e))
    }

    fn handle(&mut self, m: &Msg, pid: u32) -> Result<Msg, i64> {
        let mut path = vec![0u8; 2048];
        let text = |b: &[u8]| String::from(core::str::from_utf8(b).unwrap_or(""));
        match m.label() {
            proto::OPEN => {
                let n = ipc::unpack_at(m, 1, &mut path);
                let (fd, e) = self.open(&text(&path[..n]), ipc::header(m, 0), pid)?;
                let mut r = Msg::new(0, 3, 0);
                r.mr = [fd, e.size as u64, kind(&e), 0, 0, 0];
                Ok(r)
            }
            proto::READ => {
                let (fd, pos, len) = (m.mr[0], m.mr[1], (m.mr[2] as usize).min(proto::MAX_IO));
                let mut buf = vec![0u8; len];
                let o = self.fd(fd, pid)?;
                let (mut e, mut cur) = (o.entry.clone(), o.cur);
                let n = self.vol.read(&e, &mut cur, pos, &mut buf)?;
                let o = self.fd(fd, pid)?;
                o.cur = cur;
                e.size = o.entry.size;
                Ok(ipc::pack(0, &buf[..n]))
            }
            proto::WRITE => {
                let (fd, pos) = (ipc::header(m, 0), ipc::header(m, 1));
                let mut buf = vec![0u8; proto::MAX_IO + 64];
                let n = ipc::unpack_at(m, 2, &mut buf);
                let o = self.fd(fd, pid)?;
                if o.flags & proto::O_WRITE == 0 {
                    return Err(-EBADF);
                }
                let (mut e, mut cur) = (o.entry.clone(), o.cur);
                let w = self.vol.write(&mut e, &mut cur, pos, &buf[..n])?;
                let o = self.fd(fd, pid)?;
                o.entry = e;
                o.cur = cur;
                let mut r = Msg::new(0, 1, 0);
                r.mr[0] = w as u64;
                Ok(r)
            }
            proto::DUP => {
                self.fd(m.mr[0], pid)?.refs += 1;
                Ok(Msg::new(0, 0, 0))
            }
            proto::CLOSE => {
                let o = self.fd(m.mr[0], pid)?;
                if o.refs > 1 {
                    o.refs -= 1;
                    return Ok(Msg::new(0, 0, 0));
                }
                self.files[m.mr[0] as usize - 1] = None;
                self.vol.sync()?;
                Ok(Msg::new(0, 0, 0))
            }
            proto::STAT => {
                let n = ipc::unpack(m, &mut path);
                let e = self.vol.lookup(&text(&path[..n]))?;
                let mut r = Msg::new(0, 3, 0);
                r.mr = [e.size as u64, kind(&e), e.mtime, 0, 0, 0];
                Ok(r)
            }
            proto::READDIR => {
                let index = ipc::header(m, 0) as usize;
                let n = ipc::unpack_at(m, 1, &mut path);
                let d = self.vol.lookup(&text(&path[..n]))?;
                if !d.is_dir() {
                    return Err(-ENOTDIR);
                }
                let mut k = 0;
                let mut found = None;
                self.vol.list(d.cluster, |e| {
                    if e.name == "." || e.name == ".." {
                        return true;
                    }
                    if k == index {
                        found = Some(e.clone());
                        return false;
                    }
                    k += 1;
                    true
                })?;
                Ok(match found {
                    Some(e) => {
                        ipc::pack_at(0, &[e.size as u64, kind(&e), e.mtime], e.name.as_bytes())
                    }
                    None => ipc::pack_at(0, &[0, 0, 0], &[]),
                })
            }
            proto::MKDIR => {
                let n = ipc::unpack(m, &mut path);
                self.vol.mkdir(&text(&path[..n]))?;
                self.vol.sync()?;
                Ok(Msg::new(0, 0, 0))
            }
            proto::UNLINK => {
                let n = ipc::unpack(m, &mut path);
                let p = text(&path[..n]);
                self.retry_busy(|srv| {
                    let files = &srv.files;
                    srv.vol.unlink(&p, |e| Self::is_open(files, e))
                })?;
                self.vol.sync()?;
                Ok(Msg::new(0, 0, 0))
            }
            proto::RMDIR => {
                let n = ipc::unpack(m, &mut path);
                self.vol.rmdir(&text(&path[..n]))?;
                self.vol.sync()?;
                Ok(Msg::new(0, 0, 0))
            }
            proto::RENAME => {
                let n = ipc::unpack(m, &mut path);
                let s = text(&path[..n]);
                let (from, to) = s.split_once('\0').ok_or(-EINVAL)?;
                self.retry_busy(|srv| {
                    let files = &srv.files;
                    srv.vol.rename(from, to, |e| Self::is_open(files, e))
                })?;
                self.vol.sync()?;
                Ok(Msg::new(0, 0, 0))
            }
            proto::TRUNCATE => {
                let o = self.fd(m.mr[0], pid)?;
                if o.flags & proto::O_WRITE == 0 {
                    return Err(-EBADF);
                }
                let (mut e, mut cur) = (o.entry.clone(), o.cur);
                self.vol.truncate(&mut e, &mut cur, m.mr[1])?;
                let o = self.fd(m.mr[0], pid)?;
                o.entry = e;
                o.cur = cur;
                Ok(Msg::new(0, 0, 0))
            }
            proto::SYNC => {
                self.vol.sync()?;
                Ok(Msg::new(0, 0, 0))
            }
            proto::FSTAT => {
                let e = &self.fd(m.mr[0], pid)?.entry;
                let mut r = Msg::new(0, 3, 0);
                r.mr = [e.size as u64, kind(e), e.mtime, 0, 0, 0];
                Ok(r)
            }
            proto::STATFS => {
                let (free, total) = self.vol.statfs()?;
                let mut r = Msg::new(0, 2, 0);
                r.mr[0] = free;
                r.mr[1] = total;
                Ok(r)
            }
            _ => Err(-EINVAL),
        }
    }
}

libnx::entry!(main);

fn main() -> i32 {
    // `sd` waits for `display` and `clk` first, which the other boot
    // drivers (PCIe bring-up) keep busy: give it time.
    let blk = match ns::wait_for_timeout("blk:sd", 30_000_000_000) {
        Ok(h) => h,
        Err(_) => {
            println!("[fs] no SD card: no filesystem");
            return 0;
        }
    };
    // The card driver's DMA lands straight in it (`blk::ATTACH`).
    let io =
        SharedMemory::new(IO_SECTORS * SECTOR, CONTIGUOUS | UNCACHED | DMA32).expect("fs: buffer");
    ipc::set_send_handle(0, io.handle());
    let r = ipc::call(blk, &Msg::new(blk::ATTACH, 0, 1)).expect("fs: buffer attach");
    if r.label() != 0 {
        println!("[fs] block device refused the buffer");
        return 1;
    }
    let info = ipc::call(blk, &Msg::new(blk::INFO, 0, 0)).expect("fs: info");
    let cache = Cache::new(blk, io, info.mr[1], 2048);
    let mut vol = match Volume::mount(cache) {
        Ok(v) => v,
        Err(e) => {
            println!(
                "[fs] no FAT32 volume on card ({})",
                libnx::abi::err::name(e)
            );
            return 1;
        }
    };
    let (free, total) = vol.statfs().unwrap_or((0, 0));
    println!(
        "[fs] FAT32 mounted: {} MiB, {} MiB free",
        total >> 20,
        free >> 20
    );
    let ep = drv::publish("fs").expect("fs: publish");
    let mut srv = Server {
        vol,
        files: Vec::new(),
    };
    drv::serve(ep, |rx| {
        Some(match srv.handle(&rx.msg, rx.pid) {
            Ok(m) => m,
            Err(e) => Msg::error(e),
        })
    })
}
