//! PS4 runtime for nxk: loads PS4 modules (or PC-built x86-64 shared objects) into a guest
//! address space, binds their imports by NID, first to other guest modules (the game ships
//! its own libc.prx), then to HLE functions, and runs them on the x86-64 interpreter.

pub mod hle;
pub mod mem;
pub mod module;

use hle::{Ctx, Hle, Ret};
use mem::GuestMem;
use module::Module;
use x86emu::cpu::{Cpu, Mem, Stop};

/// Why a module could not be loaded or linked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    Format(ps4fmt::Error),
    /// The guest range at this address is taken or unmapped.
    Map(u64),
    Dynamic(&'static str),
    /// A relocation type the linker does not handle.
    Reloc(u32),
}

impl From<ps4fmt::Error> for Error {
    fn from(e: ps4fmt::Error) -> Self {
        Error::Format(e)
    }
}

impl core::fmt::Display for Error {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Error::Format(e) => write!(f, "{e}"),
            Error::Map(a) => write!(f, "guest range at {a:#x} unavailable"),
            Error::Dynamic(w) => write!(f, "dynamic tables: {w}"),
            Error::Reloc(k) => write!(f, "relocation type {k} not handled"),
        }
    }
}

/// How a run ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// An `_exit` (or equivalent) with this status.
    Exited(i32),
    /// The entry point returned.
    Returned,
    /// A call reached an import nothing provides: its library and NID.
    Missing(Option<String>, String),
    /// The CPU stopped: fault, undecodable instruction, trap, or budget.
    Cpu(Stop),
}

/// How the imports were bound.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct LinkReport {
    pub to_modules: usize,
    pub to_hle: usize,
    /// Imports with no provider: bound to a thunk that stops the process if called.
    pub missing: usize,
}

const MODULE_BASE: u64 = 0x1_0000_0000;
const MODULE_STRIDE: u64 = 0x1000_0000;
const STACK_TOP: u64 = 0x7_0000_0000;
const STACK_SIZE: u64 = 1 << 20;
const TLS_BASE: u64 = 0x7_1000_0000;
/// Return address of the entry call: a `ret` to it ends the run.
const SENTINEL: u64 = 0xdead_0000_0000;

/// A guest process: memory, one CPU, its modules and the HLE layer.
pub struct Process {
    pub cpu: Cpu,
    pub mem: GuestMem,
    pub modules: Vec<Module>,
    pub hle: Hle,
    /// What the guest wrote to fds 1 and 2.
    pub stdout: Vec<u8>,
}

impl Process {
    /// An empty process with a 1 MiB stack and a TLS block at `fs`.
    pub fn new(hle: Hle) -> Process {
        let mut mem = GuestMem::default();
        assert!(mem.map(STACK_TOP - STACK_SIZE, STACK_SIZE));
        assert!(mem.map(TLS_BASE, 0x1000));
        // fs:0 holds the TLS block's own address, as the SysV TLS ABI expects.
        assert!(mem.write(TLS_BASE, 8, TLS_BASE));
        let mut cpu = Cpu::new([0; 16], 0, 2);
        cpu.fs_base = TLS_BASE;
        Process {
            cpu,
            mem,
            modules: Vec::new(),
            hle,
            stdout: Vec::new(),
        }
    }

    /// Load a module (PS4 ELF/SELF or x86-64 shared object) at the next free base.
    ///
    /// # Args
    /// * `name` - module name; `lib` - the library its exports serve; `file` - its bytes
    ///
    /// # Returns
    /// The module's index.
    pub fn load(&mut self, name: &str, lib: &str, file: &[u8]) -> Result<usize, Error> {
        let img = ps4fmt::elf::parse(file)?;
        let base = MODULE_BASE + MODULE_STRIDE * self.modules.len() as u64;
        self.modules
            .push(module::load(name, lib, &img, base, &mut self.mem)?);
        Ok(self.modules.len() - 1)
    }

    /// Where an import of module `m`'s symbol `s` binds: a guest export, else an HLE thunk.
    fn resolve(&mut self, m: usize, s: usize) -> (u64, bool) {
        let sym = &self.modules[m].symbols[s];
        let found = self.modules.iter().enumerate().find_map(|(i, other)| {
            let lib_ok = sym.lib.as_deref().is_none_or(|l| l == other.lib);
            (i != m && lib_ok).then(|| other.export(&sym.nid)).flatten()
        });
        if let Some(a) = found {
            return (a, true);
        }
        let (nid, lib) = (sym.nid.clone(), sym.lib.clone());
        (self.hle.thunk(&nid, lib.as_deref()), false)
    }

    /// Apply every module's relocations, binding imports by NID.
    pub fn link(&mut self) -> Result<LinkReport, Error> {
        let mut report = LinkReport::default();
        for m in 0..self.modules.len() {
            for r in 0..self.modules[m].relocs.len() {
                let rel = self.modules[m].relocs[r];
                let base = self.modules[m].base;
                let value = match rel.kind {
                    0 => continue,
                    8 => base.wrapping_add(rel.addend as u64), // R_X86_64_RELATIVE
                    1 | 6 | 7 => {
                        // R_X86_64_64, GLOB_DAT, JUMP_SLOT
                        let s = rel.sym as usize;
                        let sym = self.modules[m]
                            .symbols
                            .get(s)
                            .ok_or(Error::Dynamic("symbol index"))?;
                        let target = if sym.is_import() {
                            let (a, guest) = self.resolve(m, s);
                            if guest {
                                report.to_modules += 1;
                            } else if self.hle.at(a).is_some_and(|t| t.f.is_some()) {
                                report.to_hle += 1;
                            } else {
                                report.missing += 1;
                            }
                            a
                        } else {
                            self.modules[m].symbol_addr(sym)
                        };
                        target.wrapping_add(rel.addend as u64)
                    }
                    k => return Err(Error::Reloc(k)),
                };
                let at = base.wrapping_add(rel.offset);
                if !self.mem.write(at, 8, value) {
                    return Err(Error::Map(at));
                }
            }
        }
        self.cpu.hle = self.hle.range();
        Ok(report)
    }

    /// Call module `m`'s entry point and run until it exits, returns or stops.
    ///
    /// # Args
    /// * `m` - module index; `budget` - instruction limit
    pub fn run(&mut self, m: usize, budget: u64) -> Outcome {
        // As after a `call`: the return address sits at rsp, rsp + 8 is 16-byte aligned.
        self.cpu.gpr[4] = STACK_TOP - 72;
        assert!(self.mem.write(self.cpu.gpr[4], 8, SENTINEL));
        self.cpu.rip = self.modules[m].entry;
        let end = self.cpu.steps.saturating_add(budget);
        loop {
            let left = end.saturating_sub(self.cpu.steps);
            match self.cpu.run(&mut self.mem, SENTINEL, left) {
                Ok(()) => return Outcome::Returned,
                Err(Stop::Hle(addr)) => {
                    if let Some(done) = self.call_hle(addr) {
                        return done;
                    }
                }
                Err(e) => return Outcome::Cpu(e),
            }
        }
    }

    /// Serve the HLE call at thunk `addr` and return to the guest caller.
    ///
    /// # Returns
    /// `Some` when the run is over (exit, missing import, bad return address).
    fn call_hle(&mut self, addr: u64) -> Option<Outcome> {
        let Some(t) = self.hle.at(addr).cloned() else {
            return Some(Outcome::Cpu(Stop::Hle(addr)));
        };
        let Some(f) = t.f else {
            return Some(Outcome::Missing(t.lib, t.nid));
        };
        let mut ctx = Ctx {
            cpu: &mut self.cpu,
            mem: &mut self.mem,
            stdout: &mut self.stdout,
        };
        let v = match f(&mut ctx) {
            Ret::Exit(code) => return Some(Outcome::Exited(code)),
            Ret::Value(v) => v,
        };
        // Return to the caller as the function's own `ret` would.
        self.cpu.gpr[0] = v;
        let sp = self.cpu.gpr[4];
        let Some(ra) = self.mem.read(sp, 8) else {
            return Some(Outcome::Cpu(Stop::Fault(sp)));
        };
        self.cpu.gpr[4] = sp + 8;
        if ra == SENTINEL {
            return Some(Outcome::Returned);
        }
        self.cpu.rip = ra;
        None
    }
}
