//! Console client; falls back to the kernel debug syscall.
//! Used before the server is known or if it dies.

use core::fmt::{self, Write};
use core::sync::atomic::{AtomicU32, Ordering};

use nxk_abi::proto::console as proto;

use crate::ipc;
use crate::sys;

static CONSOLE: AtomicU32 = AtomicU32::new(0);

/// Set the console server endpoint (0 = none).
pub fn set(h: u32) {
    CONSOLE.store(h, Ordering::Relaxed);
}

pub fn handle() -> u32 {
    CONSOLE.load(Ordering::Relaxed)
}

pub fn write(mut b: &[u8]) {
    let h = handle();
    if h == 0 {
        sys::debug_write(b);
        return;
    }
    while !b.is_empty() {
        let n = b.len().min(ipc::MAX_BYTES);
        let m = ipc::pack(proto::WRITE, &b[..n]);
        if ipc::call(h, &m).is_err() {
            // Server gone: fall back to the kernel console.
            set(0);
            sys::debug_write(b);
            return;
        }
        b = &b[n..];
    }
}

/// Read one input line (editing and echo done by the server).
pub fn read_line(buf: &mut [u8]) -> usize {
    let h = handle();
    if h == 0 {
        return 0;
    }
    match ipc::call(h, &ipc::Msg::new(proto::READ_LINE, 0, 0)) {
        Ok(m) => ipc::unpack(&m, buf),
        Err(_) => 0,
    }
}

/// Format buffer: one message per line rather than per fragment.
struct LineWriter {
    buf: [u8; 512],
    len: usize,
}

impl LineWriter {
    fn flush(&mut self) {
        if self.len > 0 {
            write(&self.buf[..self.len]);
            self.len = 0;
        }
    }
}

impl Write for LineWriter {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        for &b in s.as_bytes() {
            if self.len == self.buf.len() {
                self.flush();
            }
            self.buf[self.len] = b;
            self.len += 1;
        }
        Ok(())
    }
}

#[doc(hidden)]
pub fn _print(args: fmt::Arguments) {
    let mut w = LineWriter {
        buf: [0; 512],
        len: 0,
    };
    let _ = w.write_fmt(args);
    w.flush();
}

#[macro_export]
macro_rules! print {
    ($($arg:tt)*) => { $crate::console::_print(format_args!($($arg)*)) };
}

#[macro_export]
macro_rules! println {
    () => { $crate::print!("\n") };
    ($($arg:tt)*) => { $crate::console::_print(format_args!("{}\n", format_args!($($arg)*))) };
}
