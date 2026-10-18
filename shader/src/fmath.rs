//! Float functions without `std`, accurate to a few ulps; the CPU
//! reference of what the GPU's MUFU unit computes (which is itself
//! approximate: compare with a tolerance).

const LN2: f32 = core::f32::consts::LN_2;
const PI: f32 = core::f32::consts::PI;
#[allow(clippy::excessive_precision)]
const TWO_PI_HI: f32 = 6.283_203_1;
const TWO_PI_LO: f32 = -1.784_071_8e-5;

pub fn abs(x: f32) -> f32 {
    f32::from_bits(x.to_bits() & 0x7FFF_FFFF)
}

pub fn floor(x: f32) -> f32 {
    if x.is_nan() || abs(x) >= 8_388_608.0 {
        return x;
    }
    let t = x as i32 as f32;
    if t > x {
        t - 1.0
    } else {
        t
    }
}

pub fn sqrt(x: f32) -> f32 {
    if x.is_nan() || x < 0.0 {
        return f32::NAN;
    }
    if x == 0.0 || x == f32::INFINITY {
        return x;
    }
    let mut y = f32::from_bits(0x1FBD_1DF5 + (x.to_bits() >> 1));
    for _ in 0..4 {
        y = 0.5 * (y + x / y);
    }
    y
}

pub fn rcp(x: f32) -> f32 {
    1.0 / x
}

pub fn rsq(x: f32) -> f32 {
    1.0 / sqrt(x)
}

pub fn ex2(x: f32) -> f32 {
    if x.is_nan() {
        return x;
    }
    if x >= 128.0 {
        return f32::INFINITY;
    }
    if x < -126.0 {
        return 0.0;
    }
    let n = floor(x);
    let t = (x - n) * LN2;
    let mut term = 1.0f32;
    let mut sum = 1.0f32;
    for k in 1..10 {
        term *= t / k as f32;
        sum += term;
    }
    sum * f32::from_bits(((n as i32 + 127) as u32) << 23)
}

pub fn lg2(x: f32) -> f32 {
    if x.is_nan() || x < 0.0 {
        return f32::NAN;
    }
    if x == 0.0 {
        return f32::NEG_INFINITY;
    }
    if x == f32::INFINITY {
        return x;
    }
    let bits = x.to_bits();
    let mut e = (bits >> 23) as i32 - 127;
    let mut m = f32::from_bits(bits & 0x007F_FFFF | 0x3F80_0000);
    if (bits >> 23) == 0 {
        // Subnormal: scale up.
        return lg2(x * 8_388_608.0) - 23.0;
    }
    if m > core::f32::consts::SQRT_2 {
        m *= 0.5;
        e += 1;
    }
    let s = (m - 1.0) / (m + 1.0);
    let s2 = s * s;
    let mut term = s;
    let mut sum = 0.0f32;
    for k in 0..7 {
        sum += term / (2 * k + 1) as f32;
        term *= s2;
    }
    e as f32 + 2.0 * sum / LN2
}

fn sin_core(r: f32) -> f32 {
    // |r| <= pi/2.
    let r2 = r * r;
    let mut term = r;
    let mut sum = r;
    for k in 1..7 {
        term *= -r2 / ((2 * k) * (2 * k + 1)) as f32;
        sum += term;
    }
    sum
}

pub fn sin(x: f32) -> f32 {
    if !x.is_finite() {
        return f32::NAN;
    }
    let k = floor(x * (0.5 / PI) + 0.5);
    let mut r = (x - k * TWO_PI_HI) - k * TWO_PI_LO;
    if r > PI / 2.0 {
        r = PI - r;
    } else if r < -PI / 2.0 {
        r = -PI - r;
    }
    sin_core(r)
}

pub fn cos(x: f32) -> f32 {
    sin(x + PI / 2.0)
}
