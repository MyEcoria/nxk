//! Processes: spawning, waiting, process manager (init).

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU32, Ordering};

use nxk_abi::proto::pm;
use nxk_abi::spawn::{HandleGrant, SpawnArgs};

use crate::{env, ipc, sys};

static PM: AtomicU32 = AtomicU32::new(0);

pub fn set_pm(h: u32) {
    PM.store(h, Ordering::Relaxed);
}

/// Process manager handle (0: none).
pub fn pm_handle() -> u32 {
    PM.load(Ordering::Relaxed)
}

pub fn exit(code: i32) -> ! {
    sys::process_exit(code as i64)
}

/// Args and environment, in the kernel-expected shape.
#[derive(Clone, Default)]
pub struct Strings {
    /// `argc + envc` NUL-terminated strings.
    pub data: Vec<u8>,
    pub argc: u32,
    pub envc: u32,
    /// Spawn flags (`nxk_abi::spawn::SUSPENDED`).
    pub flags: u32,
}

impl Strings {
    pub fn new(args: &[&str], env: &[String]) -> Self {
        let mut data = Vec::new();
        for a in args {
            data.extend_from_slice(a.as_bytes());
            data.push(0);
        }
        for e in env {
            data.extend_from_slice(e.as_bytes());
            data.push(0);
        }
        Self {
            data,
            argc: args.len() as u32,
            envc: env.len() as u32,
            flags: 0,
        }
    }

    /// Suspended spawn: the program waits for a debugger.
    pub fn suspended(mut self) -> Self {
        self.flags |= nxk_abi::spawn::SUSPENDED as u32;
        self
    }

    /// First arg (program name).
    pub fn program(&self) -> &str {
        let end = self
            .data
            .iter()
            .position(|&c| c == 0)
            .unwrap_or(self.data.len());
        core::str::from_utf8(&self.data[..end]).unwrap_or("")
    }

    /// Message-carried form: argc, envc, flags, 0, strings.
    pub fn encode(&self) -> Vec<u8> {
        let mut v = Vec::with_capacity(16 + self.data.len());
        v.extend_from_slice(&self.argc.to_le_bytes());
        v.extend_from_slice(&self.envc.to_le_bytes());
        v.extend_from_slice(&self.flags.to_le_bytes());
        v.extend_from_slice(&0u32.to_le_bytes());
        v.extend_from_slice(&self.data);
        v
    }

    pub fn decode(b: &[u8]) -> Option<Self> {
        if b.len() < 16 {
            return None;
        }
        let word = |k: usize| u32::from_le_bytes(b[k * 4..k * 4 + 4].try_into().unwrap());
        let (argc, envc, flags) = (word(0), word(1), word(2));
        let data = b[16..].to_vec();
        if argc == 0 || data.iter().filter(|&&c| c == 0).count() != (argc + envc) as usize {
            return None;
        }
        Some(Self {
            data,
            argc,
            envc,
            flags,
        })
    }
}

/// Current environment, to pass to a child.
pub fn current_env() -> Vec<String> {
    env::vars()
        .into_iter()
        .map(|(k, v)| format!("{}={}", k, v))
        .collect()
}

pub struct Spawn<'a> {
    pub elf: &'a [u8],
    pub name: &'a str,
    pub grants: &'a [HandleGrant],
    pub arg0: u64,
    pub arg1: u64,
    pub prio: u32,
    pub cpu: Option<u8>,
    pub strings: &'a Strings,
}

/// Spawn an in-memory executable (used by init).
pub fn spawn_elf(s: &Spawn) -> Result<u32, i64> {
    let a = SpawnArgs {
        elf_ptr: s.elf.as_ptr() as u64,
        elf_len: s.elf.len() as u64,
        name_ptr: s.name.as_ptr() as u64,
        name_len: s.name.len().min(15) as u64,
        grants_ptr: s.grants.as_ptr() as u64,
        grants_len: s.grants.len() as u64,
        arg0: s.arg0,
        arg1: s.arg1,
        prio: s.prio,
        cpu: s.cpu.map(|c| c as i32).unwrap_or(-1),
        strings_ptr: s.strings.data.as_ptr() as u64,
        strings_len: s.strings.data.len() as u64,
        argc: s.strings.argc,
        envc: s.strings.envc,
        flags: s.strings.flags as u64,
    };
    sys::process_spawn(&a)
}

/// Ask the process manager to spawn a boot-image program with its args
/// (first is its name) and the current environment; returns the process handle.
pub fn spawn_args(args: &[&str]) -> Result<u32, i64> {
    spawn_with(&Strings::new(args, &current_env()))
}

pub fn spawn_with(s: &Strings) -> Result<u32, i64> {
    let h = PM.load(Ordering::Relaxed);
    if h == 0 {
        return Err(-nxk_abi::err::ENOSYS);
    }
    let payload = s.encode();
    if payload.len() > ipc::MAX_BYTES {
        return Err(-nxk_abi::err::E2BIG);
    }
    let r = ipc::call(h, &ipc::pack(pm::SPAWN, &payload))?;
    ipc::checked(r)?;
    Ok(ipc::received_handle(0))
}

/// Spawn a program with no args.
pub fn spawn(name: &str) -> Result<u32, i64> {
    spawn_args(&[name])
}

/// Available programs (space-separated names).
pub fn list(buf: &mut [u8]) -> usize {
    let h = PM.load(Ordering::Relaxed);
    if h == 0 {
        return 0;
    }
    match ipc::call(h, &ipc::Msg::new(pm::LIST, 0, 0)) {
        Ok(m) => ipc::unpack(&m, buf),
        Err(_) => 0,
    }
}

pub fn wait(h: u32) -> i64 {
    sys::process_wait(h)
}

/// Pids of live processes (not yet exited). Lets services free a
/// dead client's resources (a pid is never reused).
pub fn live_pids() -> Vec<u32> {
    use nxk_abi::info::{ProcInfo, PROCESSES, PSTATE_ALIVE};
    let mut v = alloc::vec![ProcInfo::default(); 64];
    loop {
        let bytes = unsafe {
            core::slice::from_raw_parts_mut(
                v.as_mut_ptr() as *mut u8,
                v.len() * core::mem::size_of::<ProcInfo>(),
            )
        };
        let n = sys::sysinfo(PROCESSES, bytes).unwrap_or(0) as usize;
        if n <= v.len() {
            return v[..n]
                .iter()
                .filter(|p| p.state == PSTATE_ALIVE)
                .map(|p| p.pid)
                .collect();
        }
        v.resize(n + 16, ProcInfo::default());
    }
}
