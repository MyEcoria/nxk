//! High-level emulation of system library imports: each import the guest modules do not
//! export gets a thunk address; a call there stops the CPU and runs a Rust function.

use x86emu::cpu::Cpu;

use crate::mem::GuestMem;

/// First thunk address; nothing is mapped there, the CPU stops before fetching.
pub const THUNK_BASE: u64 = 0xf_fff0_0000;
const THUNK_SIZE: u64 = 16;

/// What an HLE function does to the guest.
pub enum Ret {
    /// Return to the caller with this `rax`.
    Value(u64),
    /// The process ends with this status.
    Exit(i32),
}

/// What an HLE function sees: registers, memory and the process's output.
pub struct Ctx<'a> {
    pub cpu: &'a mut Cpu,
    pub mem: &'a mut GuestMem,
    pub stdout: &'a mut Vec<u8>,
}

impl Ctx<'_> {
    /// Integer argument `n` (0..6) of the SysV calling convention.
    pub fn arg(&self, n: usize) -> u64 {
        const REGS: [usize; 6] = [7, 6, 2, 1, 8, 9]; // rdi, rsi, rdx, rcx, r8, r9
        self.cpu.gpr[REGS[n]]
    }
}

pub type HleFn = fn(&mut Ctx) -> Ret;

/// One thunk: a known function, or an import nothing provides (reported when called).
#[derive(Clone)]
pub struct Thunk {
    pub lib: Option<String>,
    pub nid: String,
    pub name: &'static str,
    pub f: Option<HleFn>,
}

/// The table of HLE functions the runtime offers, and the thunks handed out to imports.
#[derive(Default)]
pub struct Hle {
    funcs: Vec<(&'static str, &'static str, String, HleFn)>,
    pub thunks: Vec<Thunk>,
}

impl Hle {
    /// Offer `f` as `lib`'s function `name` (matched by NID).
    pub fn add(&mut self, lib: &'static str, name: &'static str, f: HleFn) {
        self.funcs.push((lib, name, ps4fmt::nid::nid(name), f));
    }

    /// Thunk address for an import, creating the thunk on first use. Imports nothing
    /// implements get a thunk too: calling it stops the process and names the NID.
    pub fn thunk(&mut self, nid: &str, lib: Option<&str>) -> u64 {
        let idx = match self
            .thunks
            .iter()
            .position(|t| t.nid == nid && t.lib.as_deref() == lib)
        {
            Some(i) => i,
            None => {
                let f = self
                    .funcs
                    .iter()
                    .find(|(l, _, n, _)| n == nid && lib.is_none_or(|lib| lib == *l));
                self.thunks.push(Thunk {
                    lib: lib.map(str::to_owned),
                    nid: nid.to_owned(),
                    name: f.map_or("", |f| f.1),
                    f: f.map(|f| f.3),
                });
                self.thunks.len() - 1
            }
        };
        THUNK_BASE + THUNK_SIZE * idx as u64
    }

    /// The thunk at `addr`, if it is one.
    pub fn at(&self, addr: u64) -> Option<&Thunk> {
        let off = addr.checked_sub(THUNK_BASE)?;
        if off % THUNK_SIZE != 0 {
            return None;
        }
        self.thunks.get((off / THUNK_SIZE) as usize)
    }

    /// Address range the CPU must stop at.
    pub fn range(&self) -> core::ops::Range<u64> {
        THUNK_BASE..THUNK_BASE + THUNK_SIZE * self.thunks.len() as u64
    }
}

/// `write(fd, buf, len)`: fd 1 and 2 go to the process's output.
fn write(c: &mut Ctx) -> Ret {
    let (fd, buf, len) = (c.arg(0) as i32, c.arg(1), c.arg(2) as usize);
    if fd != 1 && fd != 2 {
        return Ret::Value(-9i64 as u64); // EBADF
    }
    match c.mem.slice(buf, len) {
        Some(b) => {
            c.stdout.extend_from_slice(b);
            Ret::Value(len as u64)
        }
        None => Ret::Value(-14i64 as u64), // EFAULT
    }
}

fn exit(c: &mut Ctx) -> Ret {
    Ret::Exit(c.arg(0) as i32)
}

/// The libkernel functions implemented so far.
pub fn libkernel() -> Hle {
    let mut h = Hle::default();
    h.add("libkernel", "write", write);
    h.add("libkernel", "_exit", exit);
    h
}
