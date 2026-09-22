//! Zero-capacity IPC pipes (`endpoint::PIPE`): writer blocks until a reader takes the bytes.
//! Spawned programs get pipe ends for stdio; `a | b` passes both ends of one pipe, no relay.

use crate::fmt;
use crate::io::{self, BorrowedCursor, IoSlice, IoSliceMut};
use crate::sync::Mutex;
use crate::sys::pal::abi::err::{EBADF, EPIPE};
use crate::sys::pal::abi::proto::console::WRITE;
use crate::sys::pal::abi::rights::{DUP, GRANT, RECV, SEND};
use crate::sys::pal::abi::endpoint;
use crate::sys::pal::ipc::{self, Msg};
use crate::sys::pal::{cvt, syscall};

pub struct Pipe {
    handle: u32,
    reader: bool,
    /// Reader: bytes of a message longer than the read buffer.
    pending: Mutex<(Vec<u8>, usize)>,
}

impl Pipe {
    pub(crate) fn from_handle(handle: u32, reader: bool) -> Pipe {
        Pipe { handle, reader, pending: Mutex::new((Vec::new(), 0)) }
    }

    pub(crate) fn handle(&self) -> u32 {
        self.handle
    }

    pub(crate) fn is_reader(&self) -> bool {
        self.reader
    }

    pub fn try_clone(&self) -> io::Result<Self> {
        let rights = if self.reader { RECV | GRANT | DUP } else { SEND | GRANT | DUP };
        let h = cvt(syscall::handle_dup(self.handle, rights, 0))? as u32;
        Ok(Pipe::from_handle(h, self.reader))
    }

    pub fn read(&self, buf: &mut [u8]) -> io::Result<usize> {
        read_from(self.handle, self.reader, &self.pending, buf)
    }

    pub fn read_buf(&self, cursor: BorrowedCursor<'_, u8>) -> io::Result<()> {
        crate::io::default_read_buf(|b| self.read(b), cursor)
    }

    pub fn read_vectored(&self, bufs: &mut [IoSliceMut<'_>]) -> io::Result<usize> {
        crate::io::default_read_vectored(|b| self.read(b), bufs)
    }

    pub fn is_read_vectored(&self) -> bool {
        false
    }

    pub fn read_to_end(&self, buf: &mut Vec<u8>) -> io::Result<usize> {
        let start = buf.len();
        let mut chunk = vec![0u8; ipc::MAX_BYTES];
        loop {
            let n = self.read(&mut chunk)?;
            if n == 0 {
                return Ok(buf.len() - start);
            }
            buf.extend_from_slice(&chunk[..n]);
        }
    }

    pub fn write(&self, buf: &[u8]) -> io::Result<usize> {
        if self.reader {
            return Err(io::Error::from_raw_os_error(EBADF as i32));
        }
        write_to(self.handle, buf)
    }

    pub fn write_vectored(&self, bufs: &[IoSlice<'_>]) -> io::Result<usize> {
        crate::io::default_write_vectored(|b| self.write(b), bufs)
    }

    pub fn is_write_vectored(&self) -> bool {
        false
    }

    pub fn diverge(&self) -> ! {
        panic!("pipe: unsupported conversion")
    }
}

impl Drop for Pipe {
    fn drop(&mut self) {
        syscall::handle_close(self.handle);
    }
}

impl fmt::Debug for Pipe {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Pipe").field("handle", &self.handle).field("reader", &self.reader).finish()
    }
}

/// Write into a pipe (or the console, same protocol): at most one message.
pub(crate) fn write_to(h: u32, buf: &[u8]) -> io::Result<usize> {
    if buf.is_empty() {
        return Ok(0);
    }
    let (m, n) = ipc::pack(WRITE, buf);
    match ipc::call(h, &m) {
        Ok(_) => Ok(n),
        Err(e) => Err(io::Error::from_raw_os_error((-e) as i32)),
    }
}

/// Read a pipe's read end: leftover of the previous message, else the next
/// writer message. End of stream: no writer left.
pub(crate) fn read_from(h: u32, reader: bool, pending: &Mutex<(Vec<u8>, usize)>, buf: &mut [u8]) -> io::Result<usize> {
    if !reader {
        return Err(io::Error::from_raw_os_error(EBADF as i32));
    }
    if buf.is_empty() {
        return Ok(0);
    }
    let mut p = pending.lock().unwrap_or_else(|e| e.into_inner());
    loop {
        if p.1 < p.0.len() {
            let (data, pos) = &mut *p;
            let n = buf.len().min(data.len() - *pos);
            buf[..n].copy_from_slice(&data[*pos..*pos + n]);
            *pos += n;
            return Ok(n);
        }
        let m = match ipc::recv(h) {
            Ok(m) => m,
            Err(e) if e == -EPIPE => return Ok(0),
            Err(e) => return Err(io::Error::from_raw_os_error((-e) as i32)),
        };
        if m.label() != WRITE {
            let _ = ipc::reply(&Msg::new(crate::sys::pal::abi::err::EINVAL as u32, 0, 0));
            continue;
        }
        let len = ipc::bytes_len(&m, 0);
        let n = if len <= buf.len() {
            ipc::unpack(&m, buf)
        } else {
            let mut data = vec![0u8; len];
            let k = ipc::unpack(&m, &mut data);
            data.truncate(k);
            let n = buf.len().min(k);
            buf[..n].copy_from_slice(&data[..n]);
            *p = (data, n);
            n
        };
        // The writer resumes once its bytes are copied.
        let _ = ipc::reply(&Msg::new(0, 0, 0));
        if n > 0 {
            return Ok(n);
        }
        // Empty message: wait for the next one.
    }
}

/// New pipe: (reader, writer).
pub fn pipe() -> io::Result<(Pipe, Pipe)> {
    let ep = cvt(syscall::endpoint_create(endpoint::PIPE))? as u32;
    let r = cvt(syscall::handle_dup(ep, RECV | GRANT | DUP, 0));
    let w = cvt(syscall::handle_dup(ep, SEND | GRANT | DUP, 0));
    syscall::handle_close(ep);
    match (r, w) {
        (Ok(r), Ok(w)) => Ok((Pipe::from_handle(r as u32, true), Pipe::from_handle(w as u32, false))),
        (r, w) => {
            for h in [r, w].into_iter().flatten() {
                syscall::handle_close(h as u32);
            }
            Err(io::const_error!(io::ErrorKind::OutOfMemory, "pipe: handles exhausted"))
        }
    }
}
