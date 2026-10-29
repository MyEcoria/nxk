//! MXCSR state: unmasked exceptions stop before the write, reserved bits fault, and the
//! approximate reciprocals stay within the SDM's bound of the host's.

use x86emu::cpu::{Cpu, Mem, Stop, MXCSR_RESET};

const BASE: u64 = 0x10000;
const SENTINEL: u64 = 0xdead_0000;

struct Flat(Vec<u8>);

impl Mem for Flat {
    fn read(&mut self, addr: u64, size: u8) -> Option<u64> {
        let o = addr.checked_sub(BASE)? as usize;
        let b = self.0.get(o..o + size as usize)?;
        Some(b.iter().rev().fold(0, |v, x| (v << 8) | *x as u64))
    }
    fn write(&mut self, addr: u64, size: u8, value: u64) -> bool {
        let Some(o) = addr.checked_sub(BASE).map(|o| o as usize) else {
            return false;
        };
        match self.0.get_mut(o..o + size as usize) {
            Some(b) => {
                b.copy_from_slice(&value.to_le_bytes()[..size as usize]);
                true
            }
            None => false,
        }
    }
}

/// Run `code` (a `ret` is appended) with rbx pointing at a scratch dword holding `dword`.
fn run(code: &[u8], cpu: &mut Cpu, dword: u32) -> (Result<(), Stop>, Flat) {
    let mut mem = Flat(vec![0; 0x3000]);
    mem.0[..code.len()].copy_from_slice(code);
    mem.0[code.len()] = 0xc3;
    assert!(mem.write(BASE + 0x1000, 4, dword as u64));
    let sp = BASE + 0x2ff8;
    assert!(mem.write(sp, 8, SENTINEL));
    // New code behind the CPU's back: drop what it decoded from an earlier run.
    cpu.flush_icache();
    cpu.rip = BASE;
    cpu.gpr[4] = sp;
    cpu.gpr[3] = BASE + 0x1000;
    let r = cpu.run(&mut mem, SENTINEL, 100);
    (r, mem)
}

#[test]
fn unmasked_exception_stops_before_the_write() {
    let mut cpu = Cpu::new([0; 16], BASE, 2);
    assert_eq!(cpu.mxcsr, MXCSR_RESET);
    cpu.xmm[0] = 0x3f80_0000; // 1.0
    cpu.xmm[1] = 0; // 0.0
                    // ldmxcsr [rbx] (ZE unmasked); divss xmm0, xmm1
    let code = [0x0f, 0xae, 0x13, 0xf3, 0x0f, 0x5e, 0xc1];
    let (r, _) = run(&code, &mut cpu, MXCSR_RESET & !(1 << 9));
    assert_eq!(r, Err(Stop::FpException(BASE + 3)));
    assert_eq!(cpu.xmm[0], 0x3f80_0000, "destination unchanged");
    assert_eq!(cpu.mxcsr & 0x3f, 4, "ZE recorded");
    // Masked: the infinity is written and ZE is sticky.
    let mut cpu = Cpu::new([0; 16], BASE, 2);
    cpu.xmm[0] = 0x3f80_0000;
    let (r, _) = run(&code[3..], &mut cpu, 0);
    assert_eq!(r, Ok(()));
    assert_eq!((cpu.xmm[0], cpu.mxcsr), (0x7f80_0000, MXCSR_RESET | 4));
}

#[test]
fn reserved_mxcsr_bits_fault() {
    let mut cpu = Cpu::new([0; 16], BASE, 2);
    let (r, _) = run(&[0x0f, 0xae, 0x13], &mut cpu, MXCSR_RESET | 1 << 16);
    assert_eq!(r, Err(Stop::GeneralProtection(BASE)));
    // stmxcsr [rbx] stores the current value.
    let (r, mut mem) = run(&[0x0f, 0xae, 0x1b], &mut cpu, 0);
    assert_eq!(r, Ok(()));
    assert_eq!(mem.read(BASE + 0x1000, 4), Some(MXCSR_RESET as u64));
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
#[test]
fn approximate_reciprocals_within_the_bound() {
    use core::arch::asm;
    let mut rng = 0x9e37_79b9_7f4a_7c15u64;
    for k in 0..20000u32 {
        rng ^= rng << 13;
        rng ^= rng >> 7;
        rng ^= rng << 17;
        let x = if k < 8 {
            [
                0u32,
                0x8000_0000,
                1,
                0x7f80_0000,
                0xff80_0000,
                0x7fc0_0000,
                0xbf80_0000,
                0x7f7f_ffff,
            ][k as usize]
        } else {
            rng as u32
        };
        for (op, sqrt) in [(0x53u8, false), (0x52, true)] {
            let mut cpu = Cpu::new([0; 16], BASE, 2);
            cpu.xmm[1] = x as u128;
            let (r, _) = run(&[0x0f, op, 0xc1], &mut cpu, 0);
            assert_eq!(r, Ok(()));
            let got = cpu.xmm[0] as u32;
            let want: u32;
            unsafe {
                if sqrt {
                    asm!("movd xmm1, {x:e}", "rsqrtps xmm0, xmm1", "movd {w:e}, xmm0", x = in(reg) x, w = out(reg) want, out("xmm0") _, out("xmm1") _);
                } else {
                    asm!("movd xmm1, {x:e}", "rcpps xmm0, xmm1", "movd {w:e}, xmm0", x = in(reg) x, w = out(reg) want, out("xmm0") _, out("xmm1") _);
                }
            }
            let (g, w) = (f32::from_bits(got), f32::from_bits(want));
            let ok = if w.is_nan() {
                g.is_nan()
            } else if w == 0.0 || w.is_infinite() || g == 0.0 || g.is_infinite() {
                got == want
            } else {
                // Both within 1.5 * 2^-12 of the exact value: within 2^-10 of each other.
                ((g - w) / w).abs() <= 1.0 / 1024.0
            };
            assert!(
                ok,
                "{} {x:#x}: got {got:#x} want {want:#x}",
                if sqrt { "rsqrtps" } else { "rcpps" }
            );
        }
    }
}

/// dpps between two different NaNs: the order modelled is the one an Intel E-core
/// (Crestmont) shows; P-cores of the same chip pick differently, Jaguar is unknown.
#[test]
fn dpps_nan_order_is_the_modelled_one() {
    let mut cpu = Cpu::new([0; 16], BASE, 2);
    cpu.xmm[0] = 0x000f_ffff_ffff_ffff_3ff4_b367_d38e_7d2c;
    cpu.xmm[1] = 0x7ff0_0000_0000_0001_3ff3_2121_b99f_4583;
    let (r, _) = run(&[0x66, 0x0f, 0x3a, 0x40, 0xc1, 0xff], &mut cpu, 0);
    assert_eq!(r, Ok(()));
    assert_eq!(cpu.xmm[0], 0x7ff0_0000_7ff0_0000_7ff0_0000_7ff0_0000);
}
