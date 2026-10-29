//! The 80-bit soft-float core against the host's x87: results, exception flags and C1,
//! for edge, invalid-encoding and random operands, under every precision and rounding
//! control.
#![cfg(all(target_os = "linux", target_arch = "x86_64"))]

use core::arch::asm;
use x86emu::fpu::{Cmp, F32, F64};
use x86emu::fpu80::{self, Env80, X80};

/// x87 status word bits compared: the exception flags, and C1.
const FLAGS: u16 = 0x3f;
const C1: u16 = 1 << 9;

/// `st0 = a, st1 = b`, run `$body`, then the status word and st0.
macro_rules! x87 {
    ($name:ident, $body:literal) => {
        fn $name(a: X80, b: X80, cw: u16) -> (X80, u16) {
            let (ma, mb) = (a.to_bits().to_le_bytes(), b.to_bits().to_le_bytes());
            let mut out = [0u8; 16];
            let mut sw = 0u16;
            unsafe {
                asm!(
                    "fninit",
                    "fldcw [{cw}]",
                    "fld tbyte ptr [{b}]",
                    "fld tbyte ptr [{a}]",
                    $body,
                    "fnstsw [{sw}]",
                    "fstp tbyte ptr [{out}]",
                    "fninit",
                    cw = in(reg) &cw,
                    a = in(reg) ma.as_ptr(),
                    b = in(reg) mb.as_ptr(),
                    sw = in(reg) &mut sw,
                    out = in(reg) out.as_mut_ptr(),
                    out("st(0)") _, out("st(1)") _, out("st(2)") _, out("st(3)") _,
                    out("st(4)") _, out("st(5)") _, out("st(6)") _, out("st(7)") _,
                );
            }
            (X80::from_bits(u128::from_le_bytes(out)), sw)
        }
    };
}

x87!(fadd, "fadd st, st(1)");
x87!(fsub, "fsub st, st(1)");
x87!(fmul, "fmul st, st(1)");
x87!(fdiv, "fdiv st, st(1)");
x87!(fsqrt, "fsqrt");
x87!(frndint, "frndint");
x87!(fscale, "fscale");
x87!(fprem, "fprem");
x87!(fprem1, "fprem1");
x87!(fxtract_sig, "fxtract");
x87!(fcom, "fcom st(1)");
x87!(fucom, "fucom st(1)");

fn rng(state: &mut u64) -> u64 {
    *state ^= *state << 13;
    *state ^= *state >> 7;
    *state ^= *state << 17;
    *state
}

fn x(se: u16, sig: u64) -> X80 {
    X80 { se, sig }
}

/// Edge values (each with both signs), invalid encodings, then random ones.
fn values(n: usize, state: &mut u64) -> Vec<X80> {
    let one = 1u64 << 63;
    let mut v = vec![
        x(0, 0),
        x(0, 1),
        x(0, 0x7fff_ffff_ffff_ffff),
        x(0, one), // pseudo-denormal
        x(1, one), // smallest normal
        x(1, u64::MAX),
        x(0x3fff, one), // 1.0
        x(0x3fff, one | 1),
        x(0x3fff, u64::MAX),
        x(0x3ffe, u64::MAX),
        x(0x4000, one | (1 << 62)), // 3.0
        x(0x403e, one),             // 2^63
        x(0x7ffe, u64::MAX),        // largest finite
        x(0x7fff, one),             // infinity
        x(0x7fff, one | (1 << 62)), // QNaN
        x(0x7fff, one | (1 << 62) | 0x1234),
        x(0x7fff, one | 1), // SNaN
        x(0x7fff, one | 0xffff),
        x(0x3fff, 0x4000_0000_0000_0000), // unnormal
        x(0x7fff, 0),                     // pseudo-infinity
        x(0x7fff, 1 << 62),               // pseudo-NaN
        x(0x3fbf, one | 0x5555),
        x(0x0040, one | 0x123),
    ];
    for i in 0..v.len() {
        v.push(x(v[i].se | 0x8000, v[i].sig));
    }
    for k in 0..n {
        let r = rng(state);
        let sig = rng(state) | one;
        let se = match k % 4 {
            0 => 0x3fff + (r % 64) as u16 - 32,
            1 => (r % 0x7fff) as u16,
            2 => 0x3fff,
            _ => (r % 40) as u16, // the denormal band
        };
        // Some significands with the low 11 bits clear (doubles), to hit exact cases.
        let sig = if k % 3 == 0 { sig & !0x7ff } else { sig };
        v.push(x(se | ((r >> 40) as u16 & 0x8000), sig));
    }
    v
}

/// Control words: all exceptions masked, precision 24 / 53 / 64, each rounding mode.
fn control_words() -> Vec<u16> {
    let mut cws = Vec::new();
    for pc in [0u16, 2, 3] {
        for rc in 0..4u16 {
            cws.push(0x7f | pc << 8 | rc << 10);
        }
    }
    cws
}

type Native = fn(X80, X80, u16) -> (X80, u16);

fn check(
    name: &str,
    native: Native,
    soft: &dyn Fn(X80, X80, &mut Env80) -> X80,
    n: usize,
    mask: u16,
) {
    let mut state = 0x0bad_5eed_1234_5678u64 ^ name.len() as u64;
    let vals = values(n, &mut state);
    let mut bad = 0;
    for cw in control_words() {
        for &a in &vals {
            for &b in &vals {
                let (want, sw) = native(a, b, cw);
                let mut env = Env80::from_fcw(cw);
                let got = soft(a, b, &mut env);
                let gsw = env.flags as u16 | if env.c1 { C1 } else { 0 };
                if (got, gsw & mask) != (want, sw & mask) {
                    if bad < 6 {
                        eprintln!("{name} cw {cw:#x}: {a:x?} {b:x?} -> {got:x?} sw {:#x}, want {want:x?} sw {:#x}", gsw & mask, sw & mask);
                    }
                    bad += 1;
                }
            }
        }
    }
    assert_eq!(bad, 0, "{name}: {bad} mismatches");
}

#[test]
fn arithmetic() {
    check(
        "fadd",
        fadd,
        &|a, b, e| fpu80::add(a, b, false, e),
        120,
        FLAGS | C1,
    );
    check(
        "fsub",
        fsub,
        &|a, b, e| fpu80::add(a, b, true, e),
        120,
        FLAGS | C1,
    );
    check(
        "fmul",
        fmul,
        &|a, b, e| fpu80::mul(a, b, e),
        120,
        FLAGS | C1,
    );
    check(
        "fdiv",
        fdiv,
        &|a, b, e| fpu80::div(a, b, e),
        120,
        FLAGS | C1,
    );
    check("fsqrt", fsqrt, &|a, _, e| fpu80::sqrt(a, e), 40, FLAGS | C1);
}

#[test]
fn rounding_scaling_remainders() {
    check(
        "frndint",
        frndint,
        &|a, _, e| fpu80::round_int(a, e),
        40,
        FLAGS | C1,
    );
    check(
        "fscale",
        fscale,
        &|a, b, e| fpu80::scale(a, b, e),
        60,
        FLAGS | C1,
    );
    check(
        "fxtract",
        fxtract_sig,
        &|a, _, e| fpu80::xtract(a, e).1,
        40,
        FLAGS,
    );
    // fprem: the remainder, C2 and the quotient bits (C0 = q2, C3 = q1, C1 = q0).
    for (name, native, nearest) in [
        ("fprem", fprem as Native, false),
        ("fprem1", fprem1 as Native, true),
    ] {
        check(
            name,
            native,
            &move |a, b, e| {
                let (r, c2, q) = fpu80::prem(a, b, nearest, e);
                e.c1 = q & 1 != 0;
                e.flags |=
                    (c2 as u32) << 10 | ((q as u32 >> 2) & 1) << 8 | ((q as u32 >> 1) & 1) << 14;
                r
            },
            60,
            FLAGS | C1 | 1 << 8 | 1 << 10 | 1 << 14,
        );
    }
}

#[test]
fn compares() {
    for (name, native, signaling) in [
        ("fcom", fcom as Native, true),
        ("fucom", fucom as Native, false),
    ] {
        check(
            name,
            native,
            &move |a, b, e| {
                let c = fpu80::compare(a, b, signaling, e);
                // C3 C2 C0: 000 greater, 001 less, 100 equal, 111 unordered.
                e.flags |= match c {
                    Cmp::Greater => 0,
                    Cmp::Less => 1 << 8,
                    Cmp::Equal => 1 << 14,
                    Cmp::Unordered => 1 << 8 | 1 << 10 | 1 << 14,
                };
                a
            },
            60,
            FLAGS | 1 << 8 | 1 << 10 | 1 << 14,
        );
    }
}

#[test]
fn conversions() {
    let mut state = 0x5eed_0000_0000_0001u64;
    let vals = values(300, &mut state);
    let mut bad = 0;
    for cw in control_words() {
        for &a in &vals {
            // fst m32 / m64
            for (f, bits) in [(F32, 32), (F64, 64)] {
                let mut out = 0u64;
                let mut sw = 0u16;
                let m = a.to_bits().to_le_bytes();
                unsafe {
                    if bits == 32 {
                        asm!("fninit", "fldcw [{cw}]", "fld tbyte ptr [{a}]", "fstp dword ptr [{o}]", "fnstsw [{sw}]", "fninit",
                             cw = in(reg) &cw, a = in(reg) m.as_ptr(), o = in(reg) &mut out, sw = in(reg) &mut sw, out("st(0)") _);
                    } else {
                        asm!("fninit", "fldcw [{cw}]", "fld tbyte ptr [{a}]", "fstp qword ptr [{o}]", "fnstsw [{sw}]", "fninit",
                             cw = in(reg) &cw, a = in(reg) m.as_ptr(), o = in(reg) &mut out, sw = in(reg) &mut sw, out("st(0)") _);
                    }
                }
                let mut env = Env80::from_fcw(cw);
                let got = fpu80::to_float(f, a, &mut env);
                let gsw = env.flags as u16 | if env.c1 { C1 } else { 0 };
                let want = if bits == 32 { out & 0xffff_ffff } else { out };
                if (got, gsw) != (want, sw & (FLAGS | C1)) {
                    if bad < 6 {
                        eprintln!("fst m{bits} cw {cw:#x}: {a:x?} -> {got:#x} sw {gsw:#x}, want {want:#x} sw {:#x}", sw & (FLAGS | C1));
                    }
                    bad += 1;
                }
            }
            // fistp m16 / m32 / m64 and fisttp m64
            for (bits, trunc) in [
                (16u32, false),
                (32, false),
                (64, false),
                (64, true),
                (32, true),
            ] {
                let mut out = 0u64;
                let mut sw = 0u16;
                let m = a.to_bits().to_le_bytes();
                unsafe {
                    match (bits, trunc) {
                        (16, _) => {
                            asm!("fninit", "fldcw [{cw}]", "fld tbyte ptr [{a}]", "fistp word ptr [{o}]", "fnstsw [{sw}]", "fninit",
                             cw = in(reg) &cw, a = in(reg) m.as_ptr(), o = in(reg) &mut out, sw = in(reg) &mut sw, out("st(0)") _)
                        }
                        (32, false) => {
                            asm!("fninit", "fldcw [{cw}]", "fld tbyte ptr [{a}]", "fistp dword ptr [{o}]", "fnstsw [{sw}]", "fninit",
                             cw = in(reg) &cw, a = in(reg) m.as_ptr(), o = in(reg) &mut out, sw = in(reg) &mut sw, out("st(0)") _)
                        }
                        (32, true) => {
                            asm!("fninit", "fldcw [{cw}]", "fld tbyte ptr [{a}]", "fisttp dword ptr [{o}]", "fnstsw [{sw}]", "fninit",
                             cw = in(reg) &cw, a = in(reg) m.as_ptr(), o = in(reg) &mut out, sw = in(reg) &mut sw, out("st(0)") _)
                        }
                        (_, false) => {
                            asm!("fninit", "fldcw [{cw}]", "fld tbyte ptr [{a}]", "fistp qword ptr [{o}]", "fnstsw [{sw}]", "fninit",
                             cw = in(reg) &cw, a = in(reg) m.as_ptr(), o = in(reg) &mut out, sw = in(reg) &mut sw, out("st(0)") _)
                        }
                        _ => {
                            asm!("fninit", "fldcw [{cw}]", "fld tbyte ptr [{a}]", "fisttp qword ptr [{o}]", "fnstsw [{sw}]", "fninit",
                             cw = in(reg) &cw, a = in(reg) m.as_ptr(), o = in(reg) &mut out, sw = in(reg) &mut sw, out("st(0)") _)
                        }
                    }
                }
                let mut env = Env80::from_fcw(cw);
                let got = fpu80::to_int(a, bits, trunc, &mut env);
                let gsw = env.flags as u16 | if env.c1 { C1 } else { 0 };
                let want = if bits == 64 {
                    out
                } else {
                    out & ((1 << bits) - 1)
                };
                if (got, gsw) != (want, sw & (FLAGS | C1)) {
                    if bad < 12 {
                        eprintln!("fist{} m{bits} cw {cw:#x}: {a:x?} -> {got:#x} sw {gsw:#x}, want {want:#x} sw {:#x}", if trunc { "tp" } else { "p" }, sw & (FLAGS | C1));
                    }
                    bad += 1;
                }
            }
        }
    }
    // fld m32 / m64 and fild m64: exact, flags only for SNaN and denormals.
    for k in 0..20000u64 {
        let v = rng(&mut state);
        let v = match k % 4 {
            0 => v & 0x800f_ffff_ffff_ffff,
            1 => v | 0x7ff0_0000_0000_0000,
            _ => v,
        };
        for f in [F32, F64] {
            let v = if f == F32 { v & 0xffff_ffff } else { v };
            let mut out = [0u8; 16];
            let mut sw = 0u16;
            unsafe {
                if f == F32 {
                    asm!("fninit", "fld dword ptr [{v}]", "fnstsw [{sw}]", "fstp tbyte ptr [{o}]", "fninit",
                         v = in(reg) &v, o = in(reg) out.as_mut_ptr(), sw = in(reg) &mut sw, out("st(0)") _);
                } else {
                    asm!("fninit", "fld qword ptr [{v}]", "fnstsw [{sw}]", "fstp tbyte ptr [{o}]", "fninit",
                         v = in(reg) &v, o = in(reg) out.as_mut_ptr(), sw = in(reg) &mut sw, out("st(0)") _);
                }
            }
            let mut env = Env80::from_fcw(0x37f);
            let got = fpu80::from_float(f, v, &mut env);
            let want = X80::from_bits(u128::from_le_bytes(out));
            if (got, env.flags as u16) != (want, sw & FLAGS) {
                if bad < 18 {
                    eprintln!(
                        "fld m{} {v:#x} -> {got:x?} {:#x}, want {want:x?} {:#x}",
                        if f == F32 { 32 } else { 64 },
                        env.flags,
                        sw & FLAGS
                    );
                }
                bad += 1;
            }
        }
        let mut out = [0u8; 16];
        unsafe {
            asm!("fninit", "fild qword ptr [{v}]", "fstp tbyte ptr [{o}]", "fninit", v = in(reg) &v, o = in(reg) out.as_mut_ptr(), out("st(0)") _);
        }
        assert_eq!(
            fpu80::from_int(v as i64),
            X80::from_bits(u128::from_le_bytes(out)),
            "fild {v:#x}"
        );
    }
    assert_eq!(bad, 0, "{bad} conversion mismatches");
}
