//! The sky through the day: where the sun is, what colors the light has.

use crate::meshes::normalize;

pub struct Sky {
    /// Toward the sun by day, toward the moon by night.
    pub sun_dir: [f32; 3],
    pub sun_col: [f32; 3],
    pub ambient_sky: [f32; 3],
    pub ambient_ground: [f32; 3],
    pub fog: [f32; 3],
    /// Background gradient: top and horizon.
    pub top: [f32; 3],
    pub horizon: [f32; 3],
}

fn mix(a: [f32; 3], b: [f32; 3], t: f32) -> [f32; 3] {
    [
        a[0] + (b[0] - a[0]) * t,
        a[1] + (b[1] - a[1]) * t,
        a[2] + (b[2] - a[2]) * t,
    ]
}

fn scale(a: [f32; 3], s: f32) -> [f32; 3] {
    [a[0] * s, a[1] * s, a[2] * s]
}

fn smooth(a: f32, b: f32, x: f32) -> f32 {
    let t = ((x - a) / (b - a)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// `t`: the time of day in 0..1; 0.25 is sunrise, 0.5 noon, 0.75 sunset.
pub fn sky(t: f32) -> Sky {
    let phase = (t - 0.25) * std::f32::consts::TAU;
    let elev = phase.sin();
    let sun = normalize([phase.cos() * 0.8, elev, 0.45]);
    let day = smooth(-0.12, 0.28, elev);
    // Warm near the horizon, white high up.
    let warm = 1.0 - smooth(0.05, 0.6, elev);
    let sun_col = scale(mix([1.05, 0.97, 0.88], [1.2, 0.55, 0.28], warm), day);
    let moon_dir = [-sun[0], -sun[1], -sun[2]];
    let night = 1.0 - day;
    let dir = if elev > -0.05 { sun } else { moon_dir };
    let moon_col = scale([0.10, 0.14, 0.26], night);
    let dusk = (1.0 - (elev.abs() / 0.35).min(1.0)) * 0.9;
    let sky_day = [0.50, 0.66, 0.98];
    let sky_night = [0.035, 0.05, 0.12];
    let hor_day = [0.68, 0.80, 0.94];
    let hor_dusk = [1.0, 0.55, 0.30];
    let hor_night = [0.05, 0.07, 0.15];
    let horizon = mix(
        mix(hor_night, hor_day, day),
        hor_dusk,
        dusk * (1.0 - (day - 0.5).abs() * 1.2).clamp(0.0, 1.0),
    );
    Sky {
        sun_dir: dir,
        sun_col: [
            sun_col[0] + moon_col[0],
            sun_col[1] + moon_col[1],
            sun_col[2] + moon_col[2],
        ],
        ambient_sky: scale(mix(sky_night, sky_day, day), 0.62),
        ambient_ground: scale(mix([0.02, 0.025, 0.04], [0.30, 0.26, 0.20], day), 0.55),
        fog: horizon,
        top: mix(sky_night, sky_day, day),
        horizon,
    }
}
