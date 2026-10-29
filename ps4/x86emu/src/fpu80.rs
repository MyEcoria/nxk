//! Soft-float for the x87's 80-bit extended format: arithmetic under precision control
//! (24, 53 or 64 significand bits, the 15-bit exponent range kept), conversions,
//! compares, fprem / fscale / fxtract, with the x87 status flags and C1.
//!
//! Separate from `fpu` (SSE): explicit integer bit, invalid encodings, a different NaN
//! rule. Values are unpacked to `sig / 2^126 * 2^exp`, leading bit at bit 126.

use crate::fpu::{self, Fmt, Round, DE, IE, OE, PE, UE, ZE};

/// An 80-bit value: sign and exponent, then the significand with its integer bit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct X80 {
    pub se: u16,
    pub sig: u64,
}

const BIAS: i32 = 16383;
const EMIN: i32 = 1 - BIAS;
const EMAX: i32 = BIAS;
const INT_BIT: u64 = 1 << 63;
const QUIET: u64 = 1 << 62;

impl X80 {
    pub const ZERO: X80 = X80 { se: 0, sig: 0 };
    /// The x87 "real indefinite": negative QNaN, integer and quiet bits only.
    pub const INDEFINITE: X80 = X80 {
        se: 0xffff,
        sig: INT_BIT | QUIET,
    };

    pub fn sign(self) -> bool {
        self.se & 0x8000 != 0
    }
    fn exp(self) -> u16 {
        self.se & 0x7fff
    }
    pub fn is_nan(self) -> bool {
        self.exp() == 0x7fff && self.sig & INT_BIT != 0 && self.sig << 1 != 0
    }
    pub fn is_snan(self) -> bool {
        self.is_nan() && self.sig & QUIET == 0
    }
    /// Pseudo-NaN, pseudo-infinity and unnormals: operands the x87 rejects (IE).
    pub fn is_invalid_encoding(self) -> bool {
        let e = self.exp();
        e != 0 && self.sig & INT_BIT == 0
    }
    /// A pseudo-denormal returned as a result is rewritten with exponent 1.
    fn canonical(self) -> X80 {
        if self.exp() == 0 && self.sig & INT_BIT != 0 {
            X80 {
                se: self.se | 1,
                sig: self.sig,
            }
        } else {
            self
        }
    }
    fn quiet(self) -> X80 {
        X80 {
            se: self.se,
            sig: self.sig | QUIET,
        }
    }
    fn inf(sign: bool) -> X80 {
        X80 {
            se: 0x7fff | (sign as u16) << 15,
            sig: INT_BIT,
        }
    }
    fn zero(sign: bool) -> X80 {
        X80 {
            se: (sign as u16) << 15,
            sig: 0,
        }
    }
    pub fn from_bits(v: u128) -> X80 {
        X80 {
            se: (v >> 64) as u16,
            sig: v as u64,
        }
    }
    pub fn to_bits(self) -> u128 {
        (self.se as u128) << 64 | self.sig as u128
    }
}

/// x87 rounding control and precision control, and what one operation raised.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Env80 {
    pub round: Round,
    /// Significand bits results are rounded to: 24, 53 or 64.
    pub prec: u32,
    /// Exception flags (the `fpu` bit values: IE, DE, ZE, OE, UE, PE).
    pub flags: u32,
    /// C1: set when the last rounding increased the magnitude.
    pub c1: bool,
}

impl Env80 {
    /// The controls of an x87 control word.
    pub fn from_fcw(cw: u16) -> Env80 {
        Env80 {
            round: [Round::Nearest, Round::Down, Round::Up, Round::Zero][((cw >> 10) & 3) as usize],
            prec: [24, 64, 53, 64][((cw >> 8) & 3) as usize],
            flags: 0,
            c1: false,
        }
    }
}

#[derive(Clone, Copy)]
enum Val {
    Nan,
    Bad,
    Inf(bool),
    Zero(bool),
    Num(bool, i32, u128),
}

fn jam(v: u128, n: u32) -> u128 {
    if n == 0 {
        v
    } else if n >= 128 {
        (v != 0) as u128
    } else {
        (v >> n) | ((v & ((1 << n) - 1)) != 0) as u128
    }
}

fn unpack(x: X80, env: &mut Env80) -> Val {
    let (s, e) = (x.sign(), x.exp() as i32);
    if x.is_invalid_encoding() {
        return Val::Bad;
    }
    if e == 0x7fff {
        return if x.sig << 1 == 0 {
            Val::Inf(s)
        } else {
            Val::Nan
        };
    }
    if x.sig == 0 {
        return Val::Zero(s);
    }
    if e == 0 {
        // Denormal or pseudo-denormal: sig * 2^(EMIN - 63).
        env.flags |= DE;
        let p = 63 - x.sig.leading_zeros();
        return Val::Num(s, p as i32 + EMIN - 63, (x.sig as u128) << (126 - p));
    }
    Val::Num(s, e - BIAS, (x.sig as u128) << 63)
}

/// Round `sig / 2^126 * 2^exp` to `env.prec` bits in the 80-bit format.
/// `unpack` for the stores and integer conversions, which never raise DE.
fn unpack_no_de(x: X80, env: &mut Env80) -> Val {
    let de = env.flags & DE;
    let v = unpack(x, env);
    env.flags = (env.flags & !DE) | de;
    v
}

fn round_pack(env: &mut Env80, sign: bool, mut exp: i32, mut sig: u128) -> X80 {
    let shift = 127 - env.prec;
    let rmask = (1u128 << shift) - 1;
    let half = 1u128 << (shift - 1);
    let increment = match (env.round, sign) {
        (Round::Nearest, _) => half,
        (Round::Up, false) | (Round::Down, true) => rmask,
        _ => 0,
    };
    let mut tiny = false;
    if exp < EMIN {
        tiny = exp < EMIN - 1 || sig + increment < 1 << 127;
        sig = jam(sig, (EMIN - exp) as u32);
        exp = EMIN;
    }
    let rbits = sig & rmask;
    let mut kept = (sig + increment) & !rmask;
    if env.round == Round::Nearest && rbits == half {
        kept &= !(1 << shift);
    }
    env.c1 = kept > (sig & !rmask);
    if kept >> 127 != 0 {
        kept >>= 1;
        exp += 1;
    }
    let inexact = rbits != 0;
    if tiny && inexact {
        env.flags |= UE;
    }
    if inexact {
        env.flags |= PE;
    }
    if exp > EMAX {
        env.flags |= OE | PE;
        let to_inf = match env.round {
            Round::Nearest => true,
            Round::Zero => false,
            Round::Up => !sign,
            Round::Down => sign,
        };
        env.c1 = to_inf;
        if to_inf {
            return X80::inf(sign);
        }
        let max = (u64::MAX >> (64 - env.prec)) << (64 - env.prec);
        return X80 {
            se: 0x7ffe | (sign as u16) << 15,
            sig: max,
        };
    }
    let sig64 = (kept >> 63) as u64;
    if sig64 == 0 {
        return X80::zero(sign);
    }
    let field = if sig64 & INT_BIT == 0 {
        0
    } else {
        (exp + BIAS) as u16
    };
    X80 {
        se: field | (sign as u16) << 15,
        sig: sig64,
    }
}

/// The result of an op with a NaN or an invalid encoding among its inputs.
/// x87 rule: a QNaN wins over an SNaN, otherwise the larger significand.
fn nan2(a: X80, b: X80, env: &mut Env80) -> X80 {
    if a.is_invalid_encoding() || b.is_invalid_encoding() {
        env.flags |= IE;
        return X80::INDEFINITE;
    }
    if a.is_snan() || b.is_snan() {
        env.flags |= IE;
    }
    let r = match (a.is_nan(), b.is_nan()) {
        (true, false) => a,
        (false, true) => b,
        _ if a.is_snan() != b.is_snan() => {
            if a.is_snan() {
                b
            } else {
                a
            }
        }
        // The larger significand; on a tie, the positive one.
        _ => match (b.sig | QUIET).cmp(&(a.sig | QUIET)) {
            core::cmp::Ordering::Greater => b,
            core::cmp::Ordering::Equal if a.sign() => b,
            _ => a,
        },
    };
    r.quiet()
}

fn invalid(env: &mut Env80) -> X80 {
    env.flags = (env.flags & !DE) | IE;
    X80::INDEFINITE
}

fn special(a: X80, b: X80) -> bool {
    a.is_nan() || b.is_nan() || a.is_invalid_encoding() || b.is_invalid_encoding()
}

/// `a + b`, or `a - b` when `sub`.
pub fn add(a: X80, b: X80, sub: bool, env: &mut Env80) -> X80 {
    if special(a, b) {
        return nan2(a, b, env);
    }
    let b = if sub {
        X80 {
            se: b.se ^ 0x8000,
            sig: b.sig,
        }
    } else {
        b
    };
    match (unpack(a, env), unpack(b, env)) {
        (Val::Inf(s), Val::Inf(t)) if s != t => invalid(env),
        (Val::Inf(s), _) | (_, Val::Inf(s)) => X80::inf(s),
        (Val::Zero(s), Val::Zero(t)) => {
            X80::zero(if s == t { s } else { env.round == Round::Down })
        }
        (Val::Zero(_), Val::Num(s, e, m)) | (Val::Num(s, e, m), Val::Zero(_)) => {
            round_pack(env, s, e, m)
        }
        (Val::Num(s, ea, ma), Val::Num(t, eb, mb)) => {
            let ((s, ea, ma), (t, eb, mb)) = if (ea, ma) >= (eb, mb) {
                ((s, ea, ma), (t, eb, mb))
            } else {
                ((t, eb, mb), (s, ea, ma))
            };
            let mb = jam(mb, (ea - eb).min(200) as u32);
            if s == t {
                let sum = ma + mb;
                if sum >> 127 != 0 {
                    round_pack(env, s, ea + 1, jam(sum, 1))
                } else {
                    round_pack(env, s, ea, sum)
                }
            } else {
                let diff = ma - mb;
                if diff == 0 {
                    return X80::zero(env.round == Round::Down);
                }
                let lz = diff.leading_zeros() - 1;
                round_pack(env, s, ea - lz as i32, diff << lz)
            }
        }
        _ => unreachable!(),
    }
}

pub fn mul(a: X80, b: X80, env: &mut Env80) -> X80 {
    if special(a, b) {
        return nan2(a, b, env);
    }
    let sign = (a.se ^ b.se) & 0x8000 != 0;
    match (unpack(a, env), unpack(b, env)) {
        (Val::Inf(_), Val::Zero(_)) | (Val::Zero(_), Val::Inf(_)) => invalid(env),
        (Val::Inf(_), _) | (_, Val::Inf(_)) => X80::inf(sign),
        (Val::Zero(_), _) | (_, Val::Zero(_)) => X80::zero(sign),
        (Val::Num(_, ea, ma), Val::Num(_, eb, mb)) => {
            // Inputs carry at most 64 significant bits: the 128-bit product is exact.
            let p = (ma >> 63) * (mb >> 63);
            if p >> 127 != 0 {
                round_pack(env, sign, ea + eb + 1, jam(p, 1))
            } else {
                round_pack(env, sign, ea + eb, p)
            }
        }
        _ => unreachable!(),
    }
}

pub fn div(a: X80, b: X80, env: &mut Env80) -> X80 {
    if special(a, b) {
        return nan2(a, b, env);
    }
    let sign = (a.se ^ b.se) & 0x8000 != 0;
    let before = env.flags;
    match (unpack(a, env), unpack(b, env)) {
        (Val::Inf(_), Val::Inf(_)) | (Val::Zero(_), Val::Zero(_)) => invalid(env),
        (Val::Inf(_), _) => X80::inf(sign),
        (Val::Num(..), Val::Zero(_)) => {
            env.flags = (env.flags & !DE) | (before & DE) | ZE;
            X80::inf(sign)
        }
        (Val::Zero(_), _) | (_, Val::Inf(_)) => X80::zero(sign),
        (Val::Num(_, ea, ma), Val::Num(_, eb, mb)) => {
            // floor(ma * 2^126 / mb) in two 63-bit steps, then a sticky bit.
            let (ma, mb) = (ma >> 63, mb >> 63);
            let (q1, r1) = ((ma << 63) / mb, (ma << 63) % mb);
            let (q2, r2) = ((r1 << 63) / mb, (r1 << 63) % mb);
            let q = (q1 << 63) | q2 | (r2 != 0) as u128;
            if q >> 126 == 0 {
                round_pack(env, sign, ea - eb - 1, q << 1)
            } else {
                round_pack(env, sign, ea - eb, q)
            }
        }
        _ => unreachable!(),
    }
}

pub fn sqrt(a: X80, env: &mut Env80) -> X80 {
    if special(a, a) {
        return nan2(a, a, env);
    }
    match unpack(a, env) {
        Val::Zero(s) => X80::zero(s),
        Val::Inf(false) => X80::inf(false),
        Val::Inf(true) | Val::Num(true, ..) => invalid(env),
        Val::Num(false, e, m) => {
            // Digit-by-digit root of m (64 bits, made even-exponent), then 61 more bits.
            let m = (m >> 63);
            let (n, half) = if e % 2 == 0 {
                (m << 63, e / 2)
            } else {
                (m << 64, (e - 1) / 2)
            };
            let (mut root, mut rem) = (0u128, 0u128);
            for i in (0..64).rev() {
                rem = (rem << 2) | ((n >> (2 * i)) & 3);
                let trial = (root << 2) | 1;
                root <<= 1;
                if rem >= trial {
                    rem -= trial;
                    root |= 1;
                }
            }
            for _ in 0..61 {
                rem <<= 2;
                let trial = (root << 2) | 1;
                root <<= 1;
                if rem >= trial {
                    rem -= trial;
                    root |= 1;
                }
            }
            // root = floor(sqrt(n) * 2^61); sqrt(n) = sqrt(value) * 2^63 * 2^-half.
            let p = 127 - root.leading_zeros();
            let sig = (root << (126 - p)) | (rem != 0) as u128;
            round_pack(env, false, half + p as i32 - 124, sig)
        }
        _ => unreachable!(),
    }
}

/// Integer magnitude of `±m / 2^126 * 2^e` rounded by `mode`, and whether inexact.
fn int_part(sign: bool, e: i32, m: u128, mode: Round) -> (u128, bool) {
    let k = 126 - e;
    if k <= 0 {
        return (if k < -1 { u128::MAX } else { m << (-k) as u32 }, false);
    }
    let (q, r, half) = if k >= 128 {
        (0, m, core::cmp::Ordering::Less)
    } else {
        let r = m & ((1u128 << k) - 1);
        (m >> k, r, r.cmp(&(1u128 << (k - 1))))
    };
    let up = r != 0
        && match mode {
            Round::Nearest => {
                half == core::cmp::Ordering::Greater
                    || (half == core::cmp::Ordering::Equal && q & 1 == 1)
            }
            Round::Up => !sign,
            Round::Down => sign,
            Round::Zero => false,
        };
    (q + up as u128, r != 0)
}

/// frndint: round to an integer value with `env.round` (no precision control).
pub fn round_int(a: X80, env: &mut Env80) -> X80 {
    if special(a, a) {
        return nan2(a, a, env);
    }
    match unpack(a, env) {
        Val::Num(s, e, m) if e < 63 => {
            let (q, inexact) = int_part(s, e, m, env.round);
            env.c1 = false;
            if inexact {
                env.flags |= PE;
                env.c1 = q != int_part(s, e, m, Round::Zero).0;
            }
            if q == 0 {
                return X80::zero(s);
            }
            let mut exact = Env80 {
                round: Round::Nearest,
                prec: 64,
                flags: 0,
                c1: false,
            };
            let p = 127 - q.leading_zeros();
            round_pack(&mut exact, s, p as i32, q << (126 - p))
        }
        _ => a,
    }
}

/// fist / fistp (rounding with `env.round`) and fisttp (`truncate`) to `bits` bits.
pub fn to_int(a: X80, bits: u32, truncate: bool, env: &mut Env80) -> u64 {
    let indefinite = 1u64 << (bits - 1);
    if special(a, a) {
        env.flags |= IE;
        return indefinite;
    }
    let mode = if truncate { Round::Zero } else { env.round };
    match unpack_no_de(a, env) {
        Val::Zero(_) => 0,
        Val::Inf(_) => {
            env.flags |= IE;
            indefinite
        }
        Val::Num(s, e, m) => {
            let (q, inexact) = int_part(s, e, m, mode);
            let limit = if s {
                1u128 << (bits - 1)
            } else {
                (1u128 << (bits - 1)) - 1
            };
            if q > limit {
                env.flags |= IE;
                return indefinite;
            }
            if inexact {
                env.flags |= PE;
                env.c1 = int_part(s, e, m, Round::Zero).0 != q;
            }
            let v = if s {
                (q as u64).wrapping_neg()
            } else {
                q as u64
            };
            if bits == 64 {
                v
            } else {
                v & ((1 << bits) - 1)
            }
        }
        _ => unreachable!(),
    }
}

/// fild: an integer, exactly.
pub fn from_int(v: i64) -> X80 {
    if v == 0 {
        return X80::ZERO;
    }
    let (s, m) = (v < 0, v.unsigned_abs());
    let lz = m.leading_zeros();
    X80 {
        se: ((63 - lz) as u16 + BIAS as u16) | ((s as u16) << 15),
        sig: m << lz,
    }
}

/// fld m32 / m64: exact widening; an SNaN raises IE and is quieted, a denormal raises DE.
pub fn from_float(f: Fmt, v: u64, env: &mut Env80) -> X80 {
    let sign = v & f.sign_bit() != 0;
    let frac_bits = f.frac;
    let exp_all = ((1u64 << f.exp) - 1) << frac_bits;
    let frac = v & ((1 << frac_bits) - 1);
    let e = (v & exp_all) >> frac_bits;
    if e == (1 << f.exp) - 1 {
        if frac == 0 {
            return X80::inf(sign);
        }
        if frac & (1 << (frac_bits - 1)) == 0 {
            env.flags |= IE;
        }
        return X80 {
            se: 0x7fff | (sign as u16) << 15,
            sig: INT_BIT | QUIET | frac << (63 - frac_bits),
        };
    }
    if e == 0 && frac == 0 {
        return X80::zero(sign);
    }
    let bias = (1i32 << (f.exp - 1)) - 1;
    let (exp, sig) = if e == 0 {
        env.flags |= DE;
        let p = 63 - frac.leading_zeros();
        (p as i32 + 1 - bias - frac_bits as i32, frac << (63 - p))
    } else {
        (e as i32 - bias, (frac | 1 << frac_bits) << (63 - frac_bits))
    };
    X80 {
        se: (exp + BIAS) as u16 | (sign as u16) << 15,
        sig,
    }
}

/// fst m32 / m64: round to `f` with `env.round`; MXCSR's FTZ and DAZ do not apply.
pub fn to_float(f: Fmt, a: X80, env: &mut Env80) -> u64 {
    let sign_bit = if a.sign() { f.sign_bit() } else { 0 };
    if a.is_invalid_encoding() {
        env.flags |= IE;
        return f.indefinite();
    }
    if a.is_nan() {
        if a.is_snan() {
            env.flags |= IE;
        }
        let exp_all = ((1u64 << f.exp) - 1) << f.frac;
        let payload = (a.sig << 1) >> (64 - f.frac);
        return sign_bit | exp_all | payload | (1 << (f.frac - 1));
    }
    let mut env32 = fpu::Env {
        round: env.round,
        ftz: false,
        daz: false,
        flags: 0,
    };
    let r = match unpack_no_de(a, env) {
        Val::Zero(_) => return sign_bit,
        Val::Inf(_) => return sign_bit | ((1u64 << f.exp) - 1) << f.frac,
        Val::Num(s, e, m) => {
            let sig = (m >> 64) as u64 | ((m as u64) != 0) as u64;
            let r = fpu::round_pack(f, &mut env32, s, e, sig);
            let mut toward_zero = fpu::Env {
                round: Round::Zero,
                ..env32
            };
            let t = fpu::round_pack(f, &mut toward_zero, s, e, sig);
            env.c1 = r & !f.sign_bit() > t & !f.sign_bit();
            r
        }
        _ => unreachable!(),
    };
    env.flags |= env32.flags;
    r
}

/// Result of an x87 compare.
pub use fpu::Cmp;

/// fcom (`signaling`: any NaN raises IE) and fucom (an SNaN only).
pub fn compare(a: X80, b: X80, signaling: bool, env: &mut Env80) -> Cmp {
    if special(a, b) {
        if signaling
            || a.is_snan()
            || b.is_snan()
            || a.is_invalid_encoding()
            || b.is_invalid_encoding()
        {
            env.flags |= IE;
        }
        return Cmp::Unordered;
    }
    let key = |v: Val| -> (i32, i64, u128) {
        match v {
            Val::Zero(_) => (0, 0, 0),
            Val::Inf(s) => (if s { -2 } else { 2 }, 0, 0),
            Val::Num(s, e, m) => {
                if s {
                    (-1, -(e as i64), !m)
                } else {
                    (1, e as i64, m)
                }
            }
            _ => unreachable!(),
        }
    };
    let (x, y) = (key(unpack(a, env)), key(unpack(b, env)));
    match x.cmp(&y) {
        core::cmp::Ordering::Less => Cmp::Less,
        core::cmp::Ordering::Equal => Cmp::Equal,
        core::cmp::Ordering::Greater => Cmp::Greater,
    }
}

/// fscale: `a * 2^trunc(b)`; the precision control does not apply.
pub fn scale(a: X80, b: X80, env: &mut Env80) -> X80 {
    let prec = env.prec;
    env.prec = 64;
    let r = scale_full(a, b, env);
    env.prec = prec;
    r
}

fn scale_full(a: X80, b: X80, env: &mut Env80) -> X80 {
    if special(a, b) {
        return nan2(a, b, env);
    }
    match (unpack(a, env), unpack(b, env)) {
        (Val::Zero(_), Val::Inf(false)) | (Val::Inf(_), Val::Inf(true)) => invalid(env),
        (Val::Zero(s), _) => X80::zero(s),
        (Val::Inf(s), _) => X80::inf(s),
        (Val::Num(s, ..), Val::Inf(t)) => {
            if t {
                X80::zero(s)
            } else {
                X80::inf(s)
            }
        }
        (Val::Num(s, e, m), Val::Zero(_)) => round_pack(env, s, e, m),
        (Val::Num(s, e, m), Val::Num(t, eb, mb)) => {
            let (n, _) = int_part(t, eb, mb, Round::Zero);
            let n = n.min(1 << 20) as i32;
            round_pack(env, s, e + if t { -n } else { n }, m)
        }
        _ => unreachable!(),
    }
}

/// fxtract: (exponent as a value, significand with exponent 0).
pub fn xtract(a: X80, env: &mut Env80) -> (X80, X80) {
    if special(a, a) {
        let n = nan2(a, a, env);
        return (n, n);
    }
    match unpack(a, env) {
        Val::Zero(s) => {
            env.flags |= ZE;
            (X80::inf(true), X80::zero(s))
        }
        Val::Inf(s) => (X80::inf(false), X80::inf(s)),
        Val::Num(s, e, m) => {
            let sig = X80 {
                se: BIAS as u16 | (s as u16) << 15,
                sig: (m >> 63) as u64,
            };
            (from_int(e as i64), sig)
        }
        _ => unreachable!(),
    }
}

/// fprem (`nearest` false: truncated quotient) and fprem1 (quotient rounded to nearest
/// even). Returns the remainder, C2 (the reduction is incomplete: exponents 64 or more
/// apart, reduced by 32..63 bits) and the quotient's three low bits (0 when partial).
pub fn prem(a: X80, b: X80, nearest: bool, env: &mut Env80) -> (X80, bool, u8) {
    if special(a, b) {
        return (nan2(a, b, env), false, 0);
    }
    match (unpack(a, env), unpack(b, env)) {
        (Val::Inf(_), _) | (_, Val::Zero(_)) => (invalid(env), false, 0),
        (Val::Zero(_), _) | (_, Val::Inf(_)) => (a.canonical(), false, 0),
        (Val::Num(s, ea, ma), Val::Num(_, eb, mb)) => {
            // a = ma * 2^(ea - 63), b = mb * 2^(eb - 63), ma and mb 64-bit integers.
            let (ma, mb) = (ma >> 63, mb >> 63);
            let d = ea - eb;
            if d < -1 || (d == -1 && (!nearest || ma <= mb)) {
                return (a.canonical(), false, 0);
            }
            let (mut r, mut q, mut neg, exp_base, partial);
            if d == -1 {
                // fprem1 with b/2 < |a| < |b|: quotient 1, remainder b - a (sign flipped).
                (r, q, neg, exp_base, partial) = ((mb << 1) - ma, 1u128, !s, eb - 64, false);
            } else {
                let steps = if d >= 64 { 32 + (d - 64) % 32 } else { d };
                partial = d >= 64;
                r = ma;
                q = 0;
                for i in 0..=steps {
                    if i > 0 {
                        r <<= 1;
                    }
                    q = (q << 1) | (r >= mb) as u128;
                    if r >= mb {
                        r -= mb;
                    }
                }
                neg = s;
                // r is at the scale of b, shifted by the steps left undone.
                exp_base = ea - steps - 63;
                if nearest && !partial && ((r << 1) > mb || ((r << 1) == mb && q & 1 == 1)) {
                    r = mb - r;
                    neg = !s;
                    q += 1;
                }
            }
            let qbits = if partial { 0 } else { (q & 7) as u8 };
            if r == 0 {
                return (X80::zero(s), partial, qbits);
            }
            let p = 127 - r.leading_zeros();
            let mut exact = Env80 {
                round: Round::Nearest,
                prec: 64,
                flags: 0,
                c1: false,
            };
            let v = round_pack(&mut exact, neg, exp_base + p as i32, r << (126 - p));
            env.flags |= exact.flags;
            (v, partial, qbits)
        }
        _ => unreachable!(),
    }
}
