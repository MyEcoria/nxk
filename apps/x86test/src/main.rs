//! x86emu on nxk: replays ALU vectors recorded from a native x86-64 run on a PC,
//! checks self-modifying code, times the interpreter, and runs the PC-built guest
//! programs (`ps4/guest/`) through the PS4 runtime's loader and NID linker.

use std::time::Instant;
use x86emu::cpu::{AF, CF, Cpu, FLAG_MASK, Mem};

mod vectors;

const BASE: u64 = 0x1000;
const SENTINEL: u64 = 0xdead_0000;
const SIZE: usize = 0x2000;

/// Guest memory: one flat region at `BASE`.
struct Flat(Vec<u8>);

impl Mem for Flat {
    fn read(&mut self, addr: u64, size: u8) -> Option<u64> {
        let o = addr.checked_sub(BASE)? as usize;
        let b = self.0.get(o..o + size as usize)?;
        Some(b.iter().rev().fold(0, |v, x| v << 8 | *x as u64))
    }
    fn write(&mut self, addr: u64, size: u8, value: u64) -> bool {
        let Some(o) = addr.checked_sub(BASE).map(|o| o as usize) else { return false };
        match self.0.get_mut(o..o + size as usize) {
            Some(b) => {
                b.copy_from_slice(&value.to_le_bytes()[..size as usize]);
                true
            }
            None => false,
        }
    }
}

fn machine(code: &[u8]) -> (Cpu, Flat) {
    let mut data = vec![0u8; SIZE];
    data[..code.len()].copy_from_slice(code);
    let mut mem = Flat(data);
    let sp = BASE + SIZE as u64 - 8;
    assert!(mem.write(sp, 8, SENTINEL));
    let mut gpr = [0u64; 16];
    gpr[4] = sp;
    (Cpu::new(gpr, BASE, 2), mem)
}

fn alu_vectors() -> Result<String, String> {
    for (n, &(op, w, a, b, fin, ra, rb, rf)) in vectors::ALU.iter().enumerate() {
        let code = [if w { 0x48 } else { 0x40 }, (op << 3) | 1, 0xd8, 0xc3];
        let (mut cpu, mut mem) = machine(&code);
        cpu.gpr[0] = a;
        cpu.gpr[3] = b;
        cpu.flags = 2 | fin;
        cpu.run(&mut mem, SENTINEL, 16).map_err(|e| format!("vector {n}: {e:?}"))?;
        let mask = if matches!(op, 1 | 4 | 6) { FLAG_MASK & !AF } else { FLAG_MASK };
        if (cpu.gpr[0], cpu.gpr[3], cpu.flags & mask) != (ra, rb, rf) {
            return Err(format!(
                "vector {n} op {op} w64 {w} {a:#x},{b:#x},cf {}: got {:#x},{:#x},{:#x} want {ra:#x},{rb:#x},{rf:#x}",
                fin & CF,
                cpu.gpr[0],
                cpu.gpr[3],
                cpu.flags & mask
            ));
        }
    }
    Ok(format!("{} ALU vectors identical to the PC's CPU", vectors::ALU.len()))
}

fn self_modifying() -> Result<String, String> {
    let code = [
        0xb9, 0x07, 0, 0, 0, 0x85, 0xdb, 0x75, 0x11, 0xbb, 1, 0, 0, 0, 0xc7, 0x05, 0xe8, 0xff, 0xff, 0xff, 0xb9, 0x09, 0,
        0, 0xeb, 0xe6, 0xc3,
    ];
    let (mut cpu, mut mem) = machine(&code);
    cpu.run(&mut mem, SENTINEL, 100).map_err(|e| format!("{e:?}"))?;
    if cpu.gpr[1] != 9 {
        return Err(format!("ecx {} (stale decoded instruction)", cpu.gpr[1]));
    }
    Ok("patched code re-decoded".into())
}

fn speed() -> Result<String, String> {
    let n: u32 = 1_000_000;
    let mut code = vec![0x31, 0xc0, 0xb9];
    code.extend_from_slice(&n.to_le_bytes());
    code.extend_from_slice(&[0x48, 0x01, 0xc8, 0x83, 0xe9, 0x01, 0x75, 0xf8, 0xc3]);
    let (mut cpu, mut mem) = machine(&code);
    let t = Instant::now();
    cpu.run(&mut mem, SENTINEL, 10_000_000).map_err(|e| format!("{e:?}"))?;
    let secs = t.elapsed().as_secs_f64();
    if cpu.gpr[0] != n as u64 * (n as u64 + 1) / 2 {
        return Err(format!("sum {}", cpu.gpr[0]));
    }
    Ok(format!("{:.1} MIPS ({} instructions in {:.0} ms)", 3.0 * n as f64 / secs / 1e6, 3 * n, secs * 1e3))
}

const LIBMINI: &[u8] = include_bytes!("../../../ps4/guest/bin/libmini.so");
const EXPECTED: &str = include_str!("../../../ps4/guest/bin/expected.txt");

/// Load the guest "libc" and `main`, link by NID, run, compare with the PC's native output.
fn guest(main: &[u8]) -> Result<String, String> {
    guest_with(LIBMINI, main)
}

fn guest_with(libc: &[u8], main: &[u8]) -> Result<String, String> {
    guest_expect(libc, main, EXPECTED)
}

fn guest_expect(libc: &[u8], main: &[u8], expected: &str) -> Result<String, String> {
    let mut p = ps4rt::Process::new(ps4rt::hle::libkernel());
    p.load("libmini.so", "libc", libc).map_err(|e| e.to_string())?;
    let m = p.load("main", "main", main).map_err(|e| e.to_string())?;
    let report = p.link().map_err(|e| e.to_string())?;
    let t = Instant::now();
    let outcome = p.run(m, 100_000_000);
    let secs = t.elapsed().as_secs_f64();
    let mut out = String::from_utf8_lossy(&p.stdout).into_owned();
    match outcome {
        ps4rt::Outcome::Exited(code) => out.push_str(&format!("exit {code}\n")),
        other => return Err(format!("stopped: {other:?} after {:?}", out.lines().last())),
    }
    if out != expected {
        let at = out.lines().zip(expected.lines()).position(|(a, b)| a != b);
        return Err(format!("output differs from the PC's at line {at:?}"));
    }
    Ok(format!(
        "{} lines identical to the PC's run; imports: {} to libmini, {} to HLE; {} instructions, {:.1} MIPS",
        out.lines().count(),
        report.to_modules,
        report.to_hle,
        p.cpu.steps,
        p.cpu.steps as f64 / secs / 1e6
    ))
}

fn guest_elf() -> Result<String, String> {
    guest(include_bytes!("../../../ps4/guest/bin/main.so"))
}

fn guest_sce() -> Result<String, String> {
    guest(include_bytes!("../../../ps4/guest/bin/main.sce"))
}

/// The same guests built at -O3 for the PS4's CPU (AVX, BMI1, SSE4.x).
fn guest_jaguar() -> Result<String, String> {
    guest_with(
        include_bytes!("../../../ps4/guest/bin/libmini-jaguar.so"),
        include_bytes!("../../../ps4/guest/bin/main-jaguar.so"),
    )
}

const FP_EXPECTED: &str = include_str!("../../../ps4/guest/bin/fp-expected.txt");

/// The floating-point guest, generic and for Jaguar (AVX).
fn guest_fp() -> Result<String, String> {
    guest_expect(LIBMINI, include_bytes!("../../../ps4/guest/bin/fp.so"), FP_EXPECTED)?;
    guest_expect(
        include_bytes!("../../../ps4/guest/bin/libmini-jaguar.so"),
        include_bytes!("../../../ps4/guest/bin/fp-jaguar.so"),
        FP_EXPECTED,
    )
}

fn main() {
    let tests: [(&str, fn() -> Result<String, String>); 7] = [
        ("alu", alu_vectors),
        ("smc", self_modifying),
        ("speed", speed),
        ("guest-elf", guest_elf),
        ("guest-sce", guest_sce),
        ("guest-jaguar", guest_jaguar),
        ("guest-fp", guest_fp),
    ];
    let mut failed = 0;
    for (name, test) in tests {
        match test() {
            Ok(msg) => println!("x86test {name}: ok, {msg}"),
            Err(msg) => {
                failed += 1;
                println!("x86test {name}: FAILED {msg}");
            }
        }
    }
    println!("x86test: {}/{} ok", tests.len() - failed, tests.len());
    std::process::exit(if failed == 0 { 0 } else { 1 });
}
