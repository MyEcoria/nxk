//! Soft-float with x86 SSE semantics: binary16/32/64 arithmetic, compares and conversions,
//! bit-exact under every MXCSR setting (rounding mode, FTZ, DAZ) with its sticky flags.
//!
//! Pure integer code, so the result never depends on the host FPU. Values are unpacked to
//! `sig / 2^62 * 2^exp` with the leading bit at bit 62 and a sticky bit in bit 0.

/// A binary floating-point format: exponent and fraction widths.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Fmt {
    pub exp: u32,
    pub frac: u32,
}

pub const F16: Fmt = Fmt { exp: 5, frac: 10 };
pub const F32: Fmt = Fmt { exp: 8, frac: 23 };
pub const F64: Fmt = Fmt { exp: 11, frac: 52 };

impl Fmt {
    fn bias(self) -> i32 {
        (1 << (self.exp - 1)) - 1
    }
    pub fn sign_bit(self) -> u64 {
        1 << (self.exp + self.frac)
    }
    fn exp_mask(self) -> u64 {
        ((1 << self.exp) - 1) << self.frac
    }
    fn frac_mask(self) -> u64 {
        (1 << self.frac) - 1
    }
    fn quiet_bit(self) -> u64 {
        1 << (self.frac - 1)
    }
    pub fn is_nan(self, v: u64) -> bool {
        v & self.exp_mask() == self.exp_mask() && v & self.frac_mask() != 0
    }
    pub fn is_snan(self, v: u64) -> bool {
        self.is_nan(v) && v & self.quiet_bit() == 0
    }
    /// The "real indefinite" QNaN invalid operations return: negative, quiet bit only.
    pub fn indefinite(self) -> u64 {
        self.sign_bit() | self.exp_mask() | self.quiet_bit()
    }
    fn inf(self, sign: bool) -> u64 {
        self.signed(sign, self.exp_mask())
    }
    fn zero(self, sign: bool) -> u64 {
        self.signed(sign, 0)
    }
    fn max_finite(self, sign: bool) -> u64 {
        self.signed(sign, self.exp_mask() - 1)
    }
    fn signed(self, sign: bool, mag: u64) -> u64 {
        if sign {
            mag | self.sign_bit()
        } else {
            mag
        }
    }
}

/// MXCSR rounding control.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Round {
    Nearest,
    Down,
    Up,
    Zero,
}

/// MXCSR exception flags (bits 0..6).
pub const IE: u32 = 1;
pub const DE: u32 = 2;
pub const ZE: u32 = 4;
pub const OE: u32 = 8;
pub const UE: u32 = 16;
pub const PE: u32 = 32;

/// Tininess is detected after rounding on x86 (checked by `tests/softfloat.rs`).
const TININESS_AFTER_ROUNDING: bool = true;

/// The rounding / flushing controls of one operation and the flags it raised.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Env {
    pub round: Round,
    pub ftz: bool,
    pub daz: bool,
    pub flags: u32,
}

impl Env {
    /// The controls of an MXCSR value, no flags raised yet.
    pub fn from_mxcsr(m: u32) -> Env {
        Env {
            round: [Round::Nearest, Round::Down, Round::Up, Round::Zero][((m >> 13) & 3) as usize],
            ftz: m & (1 << 15) != 0,
            daz: m & (1 << 6) != 0,
            flags: 0,
        }
    }
}

/// An unpacked operand.
#[derive(Clone, Copy)]
enum Val {
    Nan,
    Inf(bool),
    Zero(bool),
    /// Sign, exponent of bit 62, significand (bit 62 set).
    Num(bool, i32, u64),
}

/// Shift right, or-ing every bit shifted out into bit 0.
fn jam(v: u64, n: u32) -> u64 {
    if n == 0 {
        v
    } else if n >= 64 {
        (v != 0) as u64
    } else {
        (v >> n) | ((v & ((1 << n) - 1)) != 0) as u64
    }
}

/// Unpack `v`; a subnormal input raises DE, or is read as zero under DAZ.
fn unpack(f: Fmt, v: u64, env: &mut Env) -> Val {
    let sign = v & f.sign_bit() != 0;
    let (e, m) = ((v & f.exp_mask()) >> f.frac, v & f.frac_mask());
    if e == (1 << f.exp) - 1 {
        return if m == 0 { Val::Inf(sign) } else { Val::Nan };
    }
    if e == 0 {
        if m == 0 || env.daz {
            return Val::Zero(sign);
        }
        env.flags |= DE;
        let p = 63 - m.leading_zeros();
        return Val::Num(sign, p as i32 + 1 - f.bias() - f.frac as i32, m << (62 - p));
    }
    Val::Num(
        sign,
        e as i32 - f.bias(),
        (m | 1 << f.frac) << (62 - f.frac),
    )
}

/// `unpack` for the instructions that never raise DE (roundss, cvt*2si).
fn unpack_no_de(f: Fmt, v: u64, env: &mut Env) -> Val {
    let de = env.flags & DE;
    let r = unpack(f, v, env);
    env.flags = (env.flags & !DE) | de;
    r
}

/// Round `sig / 2^62 * 2^exp` to `f`, with overflow, underflow and FTZ.
pub(crate) fn round_pack(f: Fmt, env: &mut Env, sign: bool, mut exp: i32, mut sig: u64) -> u64 {
    let (bias, shift) = (f.bias(), 62 - f.frac);
    let (emin, emax) = (1 - bias, bias);
    let rmask = (1u64 << shift) - 1;
    let half = 1u64 << (shift - 1);
    let increment = match (env.round, sign) {
        (Round::Nearest, _) => half,
        (Round::Up, false) | (Round::Down, true) => rmask,
        _ => 0,
    };
    let mut tiny = false;
    if exp < emin {
        tiny = !TININESS_AFTER_ROUNDING || exp < emin - 1 || sig + increment < 1 << 63;
        sig = jam(sig, (emin - exp) as u32);
        exp = emin;
    }
    let rbits = sig & rmask;
    sig = (sig + increment) >> shift;
    if env.round == Round::Nearest && rbits == half {
        sig &= !1;
    }
    if sig >= 1 << (f.frac + 1) {
        sig >>= 1;
        exp += 1;
    }
    let inexact = rbits != 0;
    if tiny && env.ftz {
        env.flags |= UE | PE;
        return f.zero(sign);
    }
    if tiny && inexact {
        env.flags |= UE;
    }
    if inexact {
        env.flags |= PE;
    }
    if exp > emax {
        env.flags |= OE | PE;
        let to_inf = match env.round {
            Round::Nearest => true,
            Round::Zero => false,
            Round::Up => !sign,
            Round::Down => sign,
        };
        return if to_inf {
            f.inf(sign)
        } else {
            f.max_finite(sign)
        };
    }
    if sig == 0 {
        return f.zero(sign);
    }
    // The hidden bit, when present, adds one to the exponent field (0 for subnormals).
    f.signed(sign, (((exp + bias - 1) as u64) << f.frac) + sig)
}

/// The NaN result of a two-operand op with a NaN input: the first source if it is one,
/// else the second, quieted. An SNaN input raises IE.
fn nan2(f: Fmt, a: u64, b: u64, env: &mut Env) -> u64 {
    if f.is_snan(a) || f.is_snan(b) {
        env.flags |= IE;
    }
    if f.is_nan(a) {
        a | f.quiet_bit()
    } else {
        b | f.quiet_bit()
    }
}

/// An invalid operation: the indefinite QNaN, IE, and no DE (IE hides a denormal operand).
/// `env` holds one operation's (one lane's) flags, so clearing DE here is local.
fn invalid(f: Fmt, env: &mut Env) -> u64 {
    env.flags = (env.flags & !DE) | IE;
    f.indefinite()
}

/// `a + b`, or `a - b` when `sub`.
pub fn add(f: Fmt, a: u64, b: u64, sub: bool, env: &mut Env) -> u64 {
    if f.is_nan(a) || f.is_nan(b) {
        return nan2(f, a, b, env);
    }
    let b = if sub { b ^ f.sign_bit() } else { b };
    match (unpack(f, a, env), unpack(f, b, env)) {
        (Val::Inf(s), Val::Inf(t)) if s != t => invalid(f, env),
        (Val::Inf(s), _) | (_, Val::Inf(s)) => f.inf(s),
        (Val::Zero(s), Val::Zero(t)) => f.zero(if s == t { s } else { env.round == Round::Down }),
        (Val::Zero(_), Val::Num(s, e, m)) | (Val::Num(s, e, m), Val::Zero(_)) => {
            round_pack(f, env, s, e, m)
        }
        (Val::Num(s, ea, ma), Val::Num(t, eb, mb)) => {
            let ((s, ea, ma), (t, eb, mb)) = if (ea, ma) >= (eb, mb) {
                ((s, ea, ma), (t, eb, mb))
            } else {
                ((t, eb, mb), (s, ea, ma))
            };
            let mb = jam(mb, (ea - eb) as u32);
            if s == t {
                let sum = ma + mb;
                if sum >> 63 != 0 {
                    round_pack(f, env, s, ea + 1, jam(sum, 1))
                } else {
                    round_pack(f, env, s, ea, sum)
                }
            } else {
                let diff = ma - mb;
                if diff == 0 {
                    return f.zero(env.round == Round::Down);
                }
                let lz = diff.leading_zeros() - 1;
                round_pack(f, env, s, ea - lz as i32, diff << lz)
            }
        }
        _ => unreachable!("NaNs handled first"),
    }
}

pub fn mul(f: Fmt, a: u64, b: u64, env: &mut Env) -> u64 {
    if f.is_nan(a) || f.is_nan(b) {
        return nan2(f, a, b, env);
    }
    let sign = (a ^ b) & f.sign_bit() != 0;
    match (unpack(f, a, env), unpack(f, b, env)) {
        (Val::Inf(_), Val::Zero(_)) | (Val::Zero(_), Val::Inf(_)) => invalid(f, env),
        (Val::Inf(_), _) | (_, Val::Inf(_)) => f.inf(sign),
        (Val::Zero(_), _) | (_, Val::Zero(_)) => f.zero(sign),
        (Val::Num(_, ea, ma), Val::Num(_, eb, mb)) => {
            let p = ma as u128 * mb as u128;
            let mut sig = (p >> 62) as u64 | (p & ((1 << 62) - 1) != 0) as u64;
            let mut exp = ea + eb;
            if sig >> 63 != 0 {
                sig = jam(sig, 1);
                exp += 1;
            }
            round_pack(f, env, sign, exp, sig)
        }
        _ => unreachable!("NaNs handled first"),
    }
}

pub fn div(f: Fmt, a: u64, b: u64, env: &mut Env) -> u64 {
    if f.is_nan(a) || f.is_nan(b) {
        return nan2(f, a, b, env);
    }
    let sign = (a ^ b) & f.sign_bit() != 0;
    let before = env.flags;
    match (unpack(f, a, env), unpack(f, b, env)) {
        (Val::Inf(_), Val::Inf(_)) | (Val::Zero(_), Val::Zero(_)) => invalid(f, env),
        (Val::Inf(_), _) => f.inf(sign),
        (Val::Num(..), Val::Zero(_)) => {
            // Division by zero hides a denormal dividend.
            env.flags = (env.flags & !DE) | (before & DE) | ZE;
            f.inf(sign)
        }
        (Val::Zero(_), _) | (_, Val::Inf(_)) => f.zero(sign),
        (Val::Num(_, ea, ma), Val::Num(_, eb, mb)) => {
            let n = (ma as u128) << 63;
            let (q, r) = ((n / mb as u128) as u64, n % mb as u128);
            let mut sig = q | (r != 0) as u64;
            let mut exp = ea - eb - 1;
            if sig >> 63 != 0 {
                sig = jam(sig, 1);
                exp += 1;
            }
            round_pack(f, env, sign, exp, sig)
        }
        _ => unreachable!("NaNs handled first"),
    }
}

/// Floor of the square root of `n`.
fn isqrt(n: u128) -> u128 {
    let mut r = 0u128;
    let mut bit = 1u128 << 126;
    let mut n = n;
    while bit > n {
        bit >>= 2;
    }
    while bit != 0 {
        if n >= r + bit {
            n -= r + bit;
            r = (r >> 1) + bit;
        } else {
            r >>= 1;
        }
        bit >>= 2;
    }
    r
}

pub fn sqrt(f: Fmt, a: u64, env: &mut Env) -> u64 {
    if f.is_nan(a) {
        return nan2(f, a, a, env);
    }
    match unpack(f, a, env) {
        Val::Zero(s) => f.zero(s),
        Val::Inf(false) => f.inf(false),
        Val::Inf(true) | Val::Num(true, ..) => invalid(f, env),
        Val::Num(false, e, m) => {
            // sqrt(m / 2^62 * 2^e) with e made even: the root of m * 2^62 (or 2^63).
            let (n, half) = if e % 2 == 0 {
                ((m as u128) << 62, e / 2)
            } else {
                ((m as u128) << 63, (e - 1) / 2)
            };
            let r = isqrt(n);
            let mut sig = r as u64 | (r * r != n) as u64;
            let mut exp = half;
            if sig >> 63 != 0 {
                sig = jam(sig, 1);
                exp += 1;
            }
            round_pack(f, env, false, exp, sig)
        }
        Val::Nan => unreachable!("NaN handled first"),
    }
}

/// Result of an x86 compare.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cmp {
    Less,
    Equal,
    Greater,
    Unordered,
}

/// Compare `a` and `b`. IE for an SNaN, or for any NaN when `signaling`.
pub fn compare(f: Fmt, a: u64, b: u64, signaling: bool, env: &mut Env) -> Cmp {
    if f.is_nan(a) || f.is_nan(b) {
        if signaling || f.is_snan(a) || f.is_snan(b) {
            env.flags |= IE;
        }
        return Cmp::Unordered;
    }
    // A total order on the (DAZ-flushed) values: zeros equal, sign then magnitude.
    let key = |v: Val| -> i128 {
        let (s, mag) = match v {
            Val::Zero(_) => return 0,
            Val::Inf(s) => (s, 1i128 << 100),
            Val::Num(s, e, m) => (s, ((e as i128 + (1 << 20)) << 64) | m as i128),
            Val::Nan => unreachable!(),
        };
        if s {
            -mag
        } else {
            mag
        }
    };
    let (x, y) = (key(unpack(f, a, env)), key(unpack(f, b, env)));
    match x.cmp(&y) {
        core::cmp::Ordering::Less => Cmp::Less,
        core::cmp::Ordering::Equal => Cmp::Equal,
        core::cmp::Ordering::Greater => Cmp::Greater,
    }
}

/// minps / maxps: `a < b ? a : b` (`a > b` for max); a NaN in either, or two zeros,
/// give `b`. Both NaN kinds raise IE (a signaling compare).
pub fn min_max(f: Fmt, a: u64, b: u64, max: bool, env: &mut Env) -> u64 {
    let pick_a = match compare(f, a, b, true, env) {
        Cmp::Less => !max,
        Cmp::Greater => max,
        _ => false,
    };
    let v = if pick_a { a } else { b };
    // Under DAZ the operand returned is the flushed one.
    if env.daz && v & f.exp_mask() == 0 {
        v & f.sign_bit()
    } else {
        v
    }
}

/// Round to an integer value in the same format (roundss with `mode`).
pub fn round_int(f: Fmt, a: u64, mode: Round, suppress_pe: bool, env: &mut Env) -> u64 {
    if f.is_nan(a) {
        return nan2(f, a, a, env);
    }
    match unpack_no_de(f, a, env) {
        Val::Num(s, e, m) if e < f.frac as i32 => {
            let (q, inexact) = int_part(s, e, m, mode);
            if inexact && !suppress_pe {
                env.flags |= PE;
            }
            if q == 0 {
                return f.zero(s);
            }
            let p = 63 - (q as u64).leading_zeros();
            let mut exact = Env {
                round: Round::Nearest,
                ftz: false,
                daz: false,
                flags: 0,
            };
            round_pack(f, &mut exact, s, p as i32, (q as u64) << (62 - p))
        }
        Val::Zero(s) => f.zero(s),
        _ => a,
    }
}

/// Integer magnitude of `±m / 2^62 * 2^e` rounded by `mode`, and whether it was inexact.
fn int_part(sign: bool, e: i32, m: u64, mode: Round) -> (u128, bool) {
    let k = 62 - e;
    if k <= 0 {
        return (
            if k <= -64 {
                u128::MAX
            } else {
                (m as u128) << -k
            },
            false,
        );
    }
    let (q, r, half_cmp) = if k > 64 {
        (0u64, m, core::cmp::Ordering::Less)
    } else {
        let q = if k == 64 { 0 } else { m >> k };
        let r = if k == 64 { m } else { m & ((1 << k) - 1) };
        (q, r, r.cmp(&(1u64 << (k - 1))))
    };
    let up = r != 0
        && match mode {
            Round::Nearest => {
                half_cmp == core::cmp::Ordering::Greater
                    || (half_cmp == core::cmp::Ordering::Equal && q & 1 == 1)
            }
            Round::Up => !sign,
            Round::Down => sign,
            Round::Zero => false,
        };
    (q as u128 + up as u128, r != 0)
}

/// Convert to a signed `bits`-bit integer (cvt*2si / cvt*2dq); NaN and out-of-range give the
/// integer indefinite with IE.
pub fn to_int(f: Fmt, a: u64, bits: u32, truncate: bool, env: &mut Env) -> u64 {
    let indefinite = 1u64 << (bits - 1);
    if f.is_nan(a) {
        env.flags |= IE;
        return indefinite;
    }
    let mode = if truncate { Round::Zero } else { env.round };
    match unpack_no_de(f, a, env) {
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
        Val::Nan => unreachable!(),
    }
}

/// Convert a signed integer (cvtsi2ss / cvtdq2ps).
pub fn from_int(f: Fmt, v: i64, env: &mut Env) -> u64 {
    if v == 0 {
        return 0;
    }
    let (sign, mag) = (v < 0, v.unsigned_abs());
    let p = 63 - mag.leading_zeros();
    if p == 63 {
        return round_pack(f, env, sign, 63, jam(mag, 1));
    }
    round_pack(f, env, sign, p as i32, mag << (62 - p))
}

/// Convert between formats (cvtss2sd, cvtsd2ss, F16C). NaNs keep sign and top payload
/// bits and are quieted.
pub fn convert(from: Fmt, to: Fmt, a: u64, env: &mut Env) -> u64 {
    let sign = a & from.sign_bit() != 0;
    if from.is_nan(a) {
        if from.is_snan(a) {
            env.flags |= IE;
        }
        let payload = a & from.frac_mask();
        let payload = if to.frac >= from.frac {
            payload << (to.frac - from.frac)
        } else {
            payload >> (from.frac - to.frac)
        };
        return to.signed(sign, to.exp_mask() | to.quiet_bit() | payload);
    }
    match unpack(from, a, env) {
        Val::Zero(s) => to.zero(s),
        Val::Inf(s) => to.inf(s),
        Val::Num(s, e, m) => round_pack(to, env, s, e, m),
        Val::Nan => unreachable!(),
    }
}
