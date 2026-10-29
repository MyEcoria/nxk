//! The soft-float core against the host's SSE unit: results and MXCSR flags, for edge and
//! random operands, under every rounding mode with FTZ and DAZ on and off.
#![cfg(all(target_os = "linux", target_arch = "x86_64"))]

use core::arch::asm;
use x86emu::fpu::{self, Env, Fmt, F32, F64};

/// Every exception masked, flags clear, plus the controls under test.
fn mxcsr(rc: u32, ftz: bool, daz: bool) -> u32 {
    0x1f80 | (rc << 13) | ((ftz as u32) << 15) | ((daz as u32) << 6)
}

/// Define a native runner: xmm0 = a, xmm1 = b, run `$ins` under `mx`; result in xmm0 / rax.
macro_rules! native {
    ($name:ident, $ins:literal, $get:literal) => {
        fn $name(a: u64, b: u64, mx: u32) -> (u64, u32) {
            let (mut out, mut flags, saved) = (0u64, 0u32, 0u32);
            unsafe {
                asm!(
                    "stmxcsr [{saved}]",
                    "ldmxcsr [{mx}]",
                    "movq xmm0, {a}",
                    "movq xmm1, {b}",
                    $ins,
                    $get,
                    "stmxcsr [{flags}]",
                    "ldmxcsr [{saved}]",
                    a = in(reg) a,
                    b = in(reg) b,
                    mx = in(reg) &mx,
                    saved = in(reg) &saved,
                    flags = in(reg) &mut flags,
                    inout("rax") out,
                    out("xmm0") _,
                    out("xmm1") _,
                );
            }
            (out, flags & 0x3f)
        }
    };
}

native!(addss, "addss xmm0, xmm1", "movq rax, xmm0");
native!(subss, "subss xmm0, xmm1", "movq rax, xmm0");
native!(mulss, "mulss xmm0, xmm1", "movq rax, xmm0");
native!(divss, "divss xmm0, xmm1", "movq rax, xmm0");
native!(sqrtss, "sqrtss xmm0, xmm1", "movq rax, xmm0");
native!(minss, "minss xmm0, xmm1", "movq rax, xmm0");
native!(maxss, "maxss xmm0, xmm1", "movq rax, xmm0");
native!(addsd, "addsd xmm0, xmm1", "movq rax, xmm0");
native!(subsd, "subsd xmm0, xmm1", "movq rax, xmm0");
native!(mulsd, "mulsd xmm0, xmm1", "movq rax, xmm0");
native!(divsd, "divsd xmm0, xmm1", "movq rax, xmm0");
native!(sqrtsd, "sqrtsd xmm0, xmm1", "movq rax, xmm0");
native!(minsd, "minsd xmm0, xmm1", "movq rax, xmm0");
native!(cvtss2sd, "cvtss2sd xmm0, xmm1", "movq rax, xmm0");
native!(cvtsd2ss, "cvtsd2ss xmm0, xmm1", "movq rax, xmm0");
native!(cvtss2si, "cvtss2si eax, xmm1", "");
native!(cvttss2si, "cvttss2si eax, xmm1", "");
native!(cvtsd2si64, "cvtsd2si rax, xmm1", "");
native!(cvttsd2si64, "cvttsd2si rax, xmm1", "");
native!(roundss_floor, "roundss xmm0, xmm1, 9", "movq rax, xmm0");
native!(roundss_mxcsr, "roundss xmm0, xmm1, 4", "movq rax, xmm0");
native!(
    ucomiss,
    "xor eax, eax",
    "ucomiss xmm0, xmm1\n pushfq\n pop rax"
);
native!(
    comisd,
    "xor eax, eax",
    "comisd xmm0, xmm1\n pushfq\n pop rax"
);

/// Edge values of a format, then `n` random ones.
fn values(f: Fmt, rng: &mut u64, n: usize) -> Vec<u64> {
    let (sign, exp_all) = (f.sign_bit(), ((1u64 << f.exp) - 1) << f.frac);
    let frac_all = (1u64 << f.frac) - 1;
    let min_normal = 1u64 << f.frac;
    let one = ((1u64 << (f.exp - 1)) - 1) << f.frac;
    let mut v = vec![
        0,
        1,
        2,
        frac_all,
        frac_all - 1,
        min_normal,
        min_normal + 1,
        min_normal * 2 - 1,
        one,
        one + 1,
        one - 1,
        exp_all - 1,
        exp_all,
        exp_all | 1,
        exp_all | (1 << (f.frac - 1)),
        exp_all | frac_all,
        (3 << (f.frac - 2)) | (min_normal * 2),
        min_normal * 3 + 12345 % frac_all,
    ];
    for i in 0..v.len() {
        v.push(v[i] | sign);
    }
    for _ in 0..n {
        *rng ^= *rng << 13;
        *rng ^= *rng >> 7;
        *rng ^= *rng << 17;
        let mask = if f == F32 { 0xffff_ffff } else { u64::MAX };
        // Narrow exponents now and then, to hit the subnormal band and close operands.
        let x = *rng & mask;
        v.push(if (*rng).is_multiple_of(4) {
            (x & !(exp_all)) | (min_normal * (1 + (*rng >> 40) % 4))
        } else {
            x
        });
    }
    v
}

type Native = fn(u64, u64, u32) -> (u64, u32);
type Soft = fn(u64, u64, &mut Env) -> u64;

/// Compare a binary op on all pairs under all MXCSR controls.
fn check(name: &str, f: Fmt, native: Native, soft: Soft, n: usize, rmask: u64) {
    let mut rng = 0x1234_5678_9abc_def1u64 ^ name.len() as u64;
    let vals = values(f, &mut rng, n);
    let mut bad = 0;
    for rc in 0..4 {
        for (ftz, daz) in [(false, false), (true, false), (false, true), (true, true)] {
            let mx = mxcsr(rc, ftz, daz);
            for &a in &vals {
                for &b in &vals {
                    let (want, wflags) = native(a, b, mx);
                    let mut env = Env::from_mxcsr(mx);
                    let got = soft(a, b, &mut env);
                    if (got & rmask, env.flags) != (want & rmask, wflags) {
                        if bad < 8 {
                            eprintln!(
                                "{name} rc {rc} ftz {ftz} daz {daz}: {a:#x}, {b:#x} -> {:#x} flags {:#x}, want {:#x} flags {wflags:#x}",
                                got & rmask, env.flags, want & rmask
                            );
                        }
                        bad += 1;
                    }
                }
            }
        }
    }
    assert_eq!(bad, 0, "{name}: {bad} mismatches");
}

const M32: u64 = 0xffff_ffff;
const M64: u64 = u64::MAX;

#[test]
fn arithmetic_f32() {
    check(
        "addss",
        F32,
        addss,
        |a, b, e| fpu::add(F32, a, b, false, e),
        200,
        M32,
    );
    check(
        "subss",
        F32,
        subss,
        |a, b, e| fpu::add(F32, a, b, true, e),
        200,
        M32,
    );
    check(
        "mulss",
        F32,
        mulss,
        |a, b, e| fpu::mul(F32, a, b, e),
        200,
        M32,
    );
    check(
        "divss",
        F32,
        divss,
        |a, b, e| fpu::div(F32, a, b, e),
        200,
        M32,
    );
    check(
        "sqrtss",
        F32,
        sqrtss,
        |_, b, e| fpu::sqrt(F32, b, e),
        200,
        M32,
    );
    check(
        "minss",
        F32,
        minss,
        |a, b, e| fpu::min_max(F32, a, b, false, e),
        100,
        M32,
    );
    check(
        "maxss",
        F32,
        maxss,
        |a, b, e| fpu::min_max(F32, a, b, true, e),
        100,
        M32,
    );
}

#[test]
fn arithmetic_f64() {
    check(
        "addsd",
        F64,
        addsd,
        |a, b, e| fpu::add(F64, a, b, false, e),
        200,
        M64,
    );
    check(
        "subsd",
        F64,
        subsd,
        |a, b, e| fpu::add(F64, a, b, true, e),
        200,
        M64,
    );
    check(
        "mulsd",
        F64,
        mulsd,
        |a, b, e| fpu::mul(F64, a, b, e),
        200,
        M64,
    );
    check(
        "divsd",
        F64,
        divsd,
        |a, b, e| fpu::div(F64, a, b, e),
        200,
        M64,
    );
    check(
        "sqrtsd",
        F64,
        sqrtsd,
        |_, b, e| fpu::sqrt(F64, b, e),
        200,
        M64,
    );
    check(
        "minsd",
        F64,
        minsd,
        |a, b, e| fpu::min_max(F64, a, b, false, e),
        100,
        M64,
    );
}

#[test]
fn conversions() {
    check(
        "cvtss2sd",
        F32,
        cvtss2sd,
        |_, b, e| fpu::convert(F32, F64, b, e),
        300,
        M64,
    );
    check(
        "cvtsd2ss",
        F64,
        cvtsd2ss,
        |_, b, e| fpu::convert(F64, F32, b, e),
        300,
        M32,
    );
    check(
        "cvtss2si",
        F32,
        cvtss2si,
        |_, b, e| fpu::to_int(F32, b, 32, false, e),
        300,
        M32,
    );
    check(
        "cvttss2si",
        F32,
        cvttss2si,
        |_, b, e| fpu::to_int(F32, b, 32, true, e),
        300,
        M32,
    );
    check(
        "cvtsd2si64",
        F64,
        cvtsd2si64,
        |_, b, e| fpu::to_int(F64, b, 64, false, e),
        300,
        M64,
    );
    check(
        "cvttsd2si64",
        F64,
        cvttsd2si64,
        |_, b, e| fpu::to_int(F64, b, 64, true, e),
        300,
        M64,
    );
    // cvtsi2ss / cvtsi2sd: the integer `b` goes in through rax.
    fn cvtsi2ss_soft(_: u64, b: u64, e: &mut Env) -> u64 {
        fpu::from_int(F32, b as i32 as i64, e)
    }
    fn cvtsi2ss_native(_: u64, b: u64, mx: u32) -> (u64, u32) {
        cvtsi2ss_rax(b, mx)
    }
    fn cvtsi2ss_rax(v: u64, mx: u32) -> (u64, u32) {
        let (mut out, mut flags, saved) = (v, 0u32, 0u32);
        unsafe {
            asm!("stmxcsr [{s}]", "ldmxcsr [{m}]", "xorps xmm0, xmm0", "cvtsi2ss xmm0, eax", "movq rax, xmm0",
                 "stmxcsr [{f}]", "ldmxcsr [{s}]", s = in(reg) &saved, m = in(reg) &mx, f = in(reg) &mut flags,
                 inout("rax") out, out("xmm0") _);
        }
        (out, flags & 0x3f)
    }
    fn cvtsi2sd_soft(_: u64, b: u64, e: &mut Env) -> u64 {
        fpu::from_int(F64, b as i64, e)
    }
    fn cvtsi2sd_native(_: u64, b: u64, mx: u32) -> (u64, u32) {
        let (mut out, mut flags, saved) = (b, 0u32, 0u32);
        unsafe {
            asm!("stmxcsr [{s}]", "ldmxcsr [{m}]", "xorps xmm0, xmm0", "cvtsi2sd xmm0, rax", "movq rax, xmm0",
                 "stmxcsr [{f}]", "ldmxcsr [{s}]", s = in(reg) &saved, m = in(reg) &mx, f = in(reg) &mut flags,
                 inout("rax") out, out("xmm0") _);
        }
        (out, flags & 0x3f)
    }
    check("cvtsi2ss", F64, cvtsi2ss_native, cvtsi2ss_soft, 300, M32);
    check("cvtsi2sd", F64, cvtsi2sd_native, cvtsi2sd_soft, 300, M64);
}

#[test]
fn rounding_and_compares() {
    check(
        "roundss floor",
        F32,
        roundss_floor,
        |_, b, e| fpu::round_int(F32, b, fpu::Round::Down, true, e),
        300,
        M32,
    );
    check(
        "roundss mxcsr",
        F32,
        roundss_mxcsr,
        |_, b, e| {
            let r = e.round;
            fpu::round_int(F32, b, r, false, e)
        },
        300,
        M32,
    );
    fn ucomiss_soft(a: u64, b: u64, e: &mut Env) -> u64 {
        match fpu::compare(F32, a, b, false, e) {
            fpu::Cmp::Unordered => 0x45,
            fpu::Cmp::Less => 0x01,
            fpu::Cmp::Equal => 0x40,
            fpu::Cmp::Greater => 0,
        }
    }
    fn comisd_soft(a: u64, b: u64, e: &mut Env) -> u64 {
        match fpu::compare(F64, a, b, true, e) {
            fpu::Cmp::Unordered => 0x45,
            fpu::Cmp::Less => 0x01,
            fpu::Cmp::Equal => 0x40,
            fpu::Cmp::Greater => 0,
        }
    }
    // ZF, PF, CF of the flags; OF, SF and AF are cleared by the instruction.
    check("ucomiss", F32, ucomiss, ucomiss_soft, 100, 0x8d5);
    check("comisd", F64, comisd, comisd_soft, 100, 0x8d5);
}

/// Results just below the smallest normal, where tininess before and after rounding differ
/// and FTZ decides between zero and the smallest normal.
#[test]
fn tininess_band() {
    let mut cases = Vec::new();
    for k in 0..64u32 {
        // (1 - k ulp) * 2^-126 * (1 + j ulp) lands just below or at 2^-126.
        let a = 0x3f80_0000u64 - k as u64; // 1 - k ulp
        for j in 0..8u64 {
            cases.push((a, 0x0080_0000 + j));
        }
    }
    let mut bad = 0;
    for rc in 0..4 {
        for ftz in [false, true] {
            let mx = mxcsr(rc, ftz, false);
            for &(a, b) in &cases {
                let (want, wf) = mulss(a, b, mx);
                let mut env = Env::from_mxcsr(mx);
                let got = fpu::mul(F32, a, b, &mut env);
                if (got, env.flags) != (want & M32, wf) {
                    bad += 1;
                }
            }
        }
    }
    assert_eq!(bad, 0, "{bad} mismatches in the tininess band");
}
