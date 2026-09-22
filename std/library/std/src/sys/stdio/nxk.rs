//! Console server by default (WRITE messages out, READ_LINE lines in); pipes or null when spawned so.
//! Falls back to the kernel log without a console.

use crate::io;
use crate::sync::Mutex;
use crate::sys::pal::abi::err::EBADF;
use crate::sys::pal::abi::proto::console as proto;
use crate::sys::pal::abi::startup::{STDERR_NULL, STDERR_PIPE, STDIN_NULL, STDIN_PIPE, STDOUT_NULL, STDOUT_PIPE};
use crate::sys::pal::ipc::{self, Msg};
use crate::sys::pal::{startup_flags, svc, syscall};

pub const STDIN_BUF_SIZE: usize = crate::sys::io::DEFAULT_BUF_SIZE;

pub struct Stdin;
pub struct Stdout;
pub struct Stderr;

impl Stdin {
    pub const fn new() -> Stdin {
        Stdin
    }
}

impl Stdout {
    pub const fn new() -> Stdout {
        Stdout
    }
}

impl Stderr {
    pub const fn new() -> Stderr {
        Stderr
    }
}

/// Leftover of the last line (or pipe message) not yet read.
static PENDING: Mutex<(Vec<u8>, usize)> = Mutex::new((Vec::new(), 0));

impl io::Read for Stdin {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let flags = startup_flags();
        if flags & STDIN_NULL != 0 || buf.is_empty() {
            return Ok(0);
        }
        if flags & STDIN_PIPE != 0 {
            return crate::sys::pipe::read_from(svc::stdin(), true, &PENDING, buf);
        }
        let mut p = PENDING.lock().unwrap_or_else(|e| e.into_inner());
        if p.1 >= p.0.len() {
            let h = svc::stdin();
            if h == 0 {
                return Ok(0);
            }
            // One server-edited line (echo included), minus terminator.
            let r = match ipc::call(h, &Msg::new(proto::READ_LINE, 0, 0)) {
                Ok(r) => r,
                Err(_) => return Ok(0),
            };
            let mut line = vec![0u8; ipc::bytes_len(&r, 0)];
            let n = ipc::unpack(&r, &mut line);
            line.truncate(n);
            line.push(b'\n');
            *p = (line, 0);
        }
        let (line, pos) = &mut *p;
        let n = buf.len().min(line.len() - *pos);
        buf[..n].copy_from_slice(&line[*pos..*pos + n]);
        *pos += n;
        Ok(n)
    }
}

/// Write all of `b` to `h`: console (kernel-log fallback if gone)
/// or pipe (`BrokenPipe` without reader).
fn write_all(h: u32, pipe: bool, mut b: &[u8]) -> io::Result<usize> {
    let total = b.len();
    if h == 0 {
        syscall::debug_write(b);
        return Ok(total);
    }
    while !b.is_empty() {
        match crate::sys::pipe::write_to(h, b) {
            Ok(n) => b = &b[n..],
            Err(e) if pipe => {
                return if b.len() < total { Ok(total - b.len()) } else { Err(e) };
            }
            Err(_) => {
                // Console server gone: the rest goes to the log.
                if h == svc::console() {
                    svc::set_console(0);
                }
                syscall::debug_write(b);
                break;
            }
        }
    }
    Ok(total)
}

impl io::Write for Stdout {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let flags = startup_flags();
        if flags & STDOUT_NULL != 0 {
            return Ok(buf.len());
        }
        write_all(svc::console(), flags & STDOUT_PIPE != 0, buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl io::Write for Stderr {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let flags = startup_flags();
        if flags & STDERR_NULL != 0 {
            return Ok(buf.len());
        }
        write_all(svc::stderr(), flags & STDERR_PIPE != 0, buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

pub fn is_ebadf(err: &io::Error) -> bool {
    err.raw_os_error() == Some(EBADF as i32)
}

pub fn panic_output() -> Option<impl io::Write> {
    Some(Stderr::new())
}
