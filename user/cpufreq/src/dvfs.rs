//! CPU rail voltage and PLLX dividers per sys-clk step, as NVIDIA's L4T
//! kernel computes them for a PLLX-sourced CPU on the Switch Erista
//! (`tegra210-dvfs.c`: CPU_PLL_CVB_TABLE_ODN, speedo ID 3, max 1227 mV).
//! Pure, tested on the host:
//! `rustc --edition 2021 --test user/cpufreq/src/dvfs.rs -o dvfs && ./dvfs`.

/// PLL-source CVB coefficients (c0, c1, c2) per step of `STEPS_MHZ`:
/// µV at speedo scale 100; none below 1020 MHz (the floor applies).
const PLL_CVB: [[i64; 3]; 12] = [
    [0, 0, 0],
    [0, 0, 0],
    [0, 0, 0],
    [0, 0, 0],
    [-2_875_621, 358_099, -8_585],
    [-52_225, 104_159, -2_816],
    [1_076_868, 8_356, -727],
    [2_208_191, -84_659, 1_240],
    [2_519_460, -105_063, 1_611],
    [2_889_664, -122_173, 1_834],
    [5_100_873, -279_186, 4_747],
    [5_100_873, -279_186, 4_747],
];
/// L4T's CVB_PLL_MARGIN: a PLL cannot follow rail droop as the DFLL does.
const PLL_MARGIN_PCT: i64 = 30;
/// pll_min_millivolts.
pub const MIN_UV: u32 = 950_000;
/// max_mv 1227, rounded down to the rail's grid.
pub const MAX_UV: u32 = 1_225_000;
/// MAX77621 VOUT code 0 and step (hekate bdk/power/max7762x.c).
pub const RAIL_BASE_UV: u32 = 606_250;
pub const RAIL_STEP_UV: u32 = 6_250;

/// Linux DIV_ROUND_CLOSEST on signed values.
fn div_round_closest(x: i64, d: i64) -> i64 {
    if (x > 0) == (d > 0) {
        (x + d / 2) / d
    } else {
        (x - d / 2) / d
    }
}

/// Linux `speedo-tegra210.c`: the CPU speedo value from CPU_SPEEDO_0 and
/// the speedo fusing revision; `None` if unfused.
pub fn speedo(rev: u32, cpu_speedo_0: u32) -> Option<u32> {
    let v = match rev {
        3.. => cpu_speedo_0 as i64,
        2 => (-1938 + 1095 * cpu_speedo_0 as i64 / 100) / 10,
        _ => 2100,
    };
    (v > 0).then_some(v as u32)
}

/// VOUT code for `uv`, rounded up to the grid.
pub fn rail_code(uv: u32) -> u8 {
    uv.saturating_sub(RAIL_BASE_UV).div_ceil(RAIL_STEP_UV) as u8
}

pub fn rail_uv(code: u8) -> u32 {
    RAIL_BASE_UV + code as u32 * RAIL_STEP_UV
}

/// Rail voltage (µV, on the grid) step `i` needs on this chip; may
/// exceed `MAX_UV`.
pub fn voltage_uv(speedo: u32, i: usize) -> u32 {
    let [c0, c1, c2] = PLL_CVB[i];
    let s = speedo as i64;
    let v = div_round_closest(c2 * s, 100);
    let v = div_round_closest((v + c1) * s, 100) + c0;
    let v = (100 + PLL_MARGIN_PCT) * v / 100;
    rail_uv(rail_code(v.max(0) as u32)).max(MIN_UV)
}

/// Steps this chip runs within `MAX_UV` (bit i: step i).
pub fn allowed(speedo: u32) -> u64 {
    (0..PLL_CVB.len())
        .filter(|&i| voltage_uv(speedo, i) <= MAX_UV)
        .fold(0, |m, i| m | 1 << i)
}

/// PLLX output divider per hardware code (Linux pll_qlin_pdiv_to_hw).
const PDIV: [u64; 15] = [1, 2, 3, 4, 5, 6, 8, 10, 12, 16, 12, 16, 20, 24, 32];
pub const OSC_KHZ: u64 = 38_400;

/// PLLX dividers, as PLLX_BASE fields.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Pll {
    pub m: u32,
    pub n: u32,
    pub p: u32,
}

impl Pll {
    pub fn from_base(base: u32) -> Self {
        Self {
            m: base & 0xFF,
            n: (base >> 8) & 0xFF,
            p: (base >> 20) & 0x1F,
        }
    }

    /// The divider bits of PLLX_BASE.
    pub fn base(self) -> u32 {
        self.p << 20 | self.n << 8 | self.m
    }

    pub fn khz(self) -> u64 {
        match PDIV.get(self.p as usize) {
            Some(&p) if self.m != 0 => OSC_KHZ * self.n as u64 / (self.m as u64 * p),
            _ => 0,
        }
    }
}

/// Highest PLLX output at or below `mhz`: comparison frequency 38.4 or
/// 19.2 MHz (M = 1, 2, as hekate), VCO 1350-3000 MHz (Linux pll_x_params).
pub fn pllx(mhz: u64) -> Option<Pll> {
    let mut best: Option<Pll> = None;
    for m in 1..=2u64 {
        for (code, &p) in PDIV.iter().enumerate() {
            let n = (mhz * 1000 * m * p / OSC_KHZ).min(255);
            if !(1_350_000 * m..=3_000_000 * m).contains(&(OSC_KHZ * n)) {
                continue;
            }
            let pll = Pll {
                m: m as u32,
                n: n as u32,
                p: code as u32,
            };
            if best.is_none_or(|b| pll.khz() > b.khz()) {
                best = Some(pll);
            }
        }
    }
    best
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Python port of L4T's integer arithmetic, for three chips.
    #[test]
    fn voltages() {
        let mv = |s| (0..12).map(|i| voltage_uv(s, i) / 1000).collect::<Vec<_>>();
        let floor = [950, 950, 950, 950];
        let hi = |v: &[u32]| [&floor[..], v].concat();
        assert_eq!(
            mv(1950),
            hi(&[1100, 1181, 1256, 1343, 1412, 1568, 1906, 1906])
        );
        assert_eq!(
            mv(2100),
            hi(&[1118, 1162, 1212, 1275, 1331, 1475, 1731, 1731])
        );
        assert_eq!(
            mv(2250),
            hi(&[1087, 1131, 1168, 1212, 1262, 1393, 1593, 1593])
        );
        assert_eq!(voltage_uv(2100, 4), 1_118_750);
    }

    #[test]
    fn steps() {
        assert_eq!(allowed(1950), 0b11_1111);
        assert_eq!(allowed(2100), 0b111_1111);
        assert_eq!(allowed(2250), 0b1111_1111);
    }

    #[test]
    fn rail() {
        assert_eq!(rail_code(950_000), 55);
        assert_eq!(rail_uv(55), 950_000);
        assert_eq!(rail_code(1_118_700), 82);
        assert_eq!(rail_code(MAX_UV), 99);
    }

    #[test]
    fn speedo_revisions() {
        assert_eq!(speedo(3, 2034), Some(2034));
        assert_eq!(speedo(2, 2000), Some((21900 - 1938) / 10));
        assert_eq!(speedo(0, 0), Some(2100));
        assert_eq!(speedo(3, 0), None);
    }

    #[test]
    fn dividers() {
        // hekate's boot setting: M 2, N 156, P code 2 (/3).
        let boot = Pll::from_base(2 << 20 | 156 << 8 | 2);
        assert_eq!(boot.khz(), 998_400);
        assert_eq!(Pll::from_base(boot.base()), boot);
        let khz = |m| pllx(m).unwrap().khz();
        let got: Vec<u64> = [
            612, 714, 816, 918, 1020, 1122, 1224, 1326, 1428, 1581, 1683, 1785,
        ]
        .iter()
        .map(|&m| khz(m))
        .collect();
        assert_eq!(
            got,
            [
                609_600, 710_400, 816_000, 915_200, 1_017_600, 1_113_600, 1_219_200, 1_324_800,
                1_420_800, 1_574_400, 1_670_400, 1_766_400
            ]
        );
        assert_eq!(pllx(1224), Some(Pll { m: 2, n: 127, p: 1 }));
    }
}
