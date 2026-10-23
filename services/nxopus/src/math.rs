//! The float functions libopus's float build uses (without FLOAT_APPROX).

pub fn exp2(x: f32) -> f32 {
    (std::f64::consts::LN_2 * x as f64).exp() as f32
}
