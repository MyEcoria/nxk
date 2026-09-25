//! GPCPLL arithmetic and GPU rail voltage for the GM20B in noise-aware
//! (NA) mode, as NVIDIA's gm20b clock code carried by nouveau
//! (nvkm/subdev/clk/gm20b.c, gk20a.c, volt/gm20b.c) computes them.
//! Pure, tested on the host:
//! `rustc --edition 2021 --test user/gpu/src/pll.rs -o pll && ./pll`.

/// PLLG_REF: the 38.4 MHz oscillator.
pub const REF_KHZ: u32 = 38_400;
/// `gm20b_pllg_params`: VCO, update rate (U = REF / M), M, N, PL ranges.
const MIN_VCO: u32 = 1_300_000;
const MAX_VCO: u32 = 2_600_000;
const MIN_U: u32 = 12_000;
const MAX_U: u32 = 38_400;
const MIN_N: u32 = 8;
const MAX_N: u32 = 255;
const MIN_PL: u32 = 1;
const MAX_PL: u32 = 31;
/// NA mode runs at the highest update rate: M fixed to REF / MAX_U.
const NA_M: u32 = REF_KHZ.div_ceil(MAX_U);

/// Power-up clock: nouveau's lowest GM20B pstate, sys-clk's first step.
pub const GPC_KHZ: u32 = 76_800;

/// PLL coefficients: VCO = REF * N / M, GPC clock = VCO / PL / 2.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Mnp {
    pub m: u32,
    pub n: u32,
    pub pl: u32,
}

impl Mnp {
    pub fn from_coeff(v: u32) -> Self {
        Self {
            m: v & 0xFF,
            n: (v >> 8) & 0xFF,
            pl: (v >> 16) & 0x3F,
        }
    }

    /// GPCPLL_COEFF.
    pub fn coeff(self) -> u32 {
        self.pl << 16 | self.n << 8 | self.m
    }

    pub fn khz(self) -> u32 {
        if self.m == 0 || self.pl == 0 {
            return 0;
        }
        (REF_KHZ as u64 * self.n as u64 / (self.m as u64 * self.pl as u64) / 2) as u32
    }

    /// NDIV at the bottom of the VCO range for this M (`gk20a_pllg_n_lo`).
    pub fn n_lo(self) -> u32 {
        (self.m * MIN_VCO).div_ceil(REF_KHZ)
    }
}

/// `gk20a_pllg_calc_mnp` with NA mode's fixed M: the lowest VCO that
/// gives `khz` exactly or closest.
pub fn calc(khz: u32) -> Mnp {
    let target = khz * 2;
    let mut target_vco = target + target / 50;
    let max_vco = MAX_VCO.max(target_vco);
    let high_pl = (max_vco + target_vco - 1)
        .div_euclid(target_vco)
        .clamp(MIN_PL, MAX_PL);
    let low_pl = (MIN_VCO / target_vco).clamp(MIN_PL, MAX_PL);
    let mut best = Mnp {
        m: NA_M,
        n: MIN_N,
        pl: MIN_PL,
    };
    let mut best_delta = u32::MAX;
    'search: for pl in low_pl..=high_pl {
        target_vco = target * pl;
        let m = NA_M;
        let u = REF_KHZ / m;
        if !(MIN_U..=MAX_U).contains(&u) {
            continue;
        }
        let n0 = target_vco * m / REF_KHZ;
        let n2 = (target_vco * m).div_ceil(REF_KHZ);
        if n0 > MAX_N {
            break;
        }
        for n in n0.max(MIN_N)..=n2.min(MAX_N) {
            let vco = REF_KHZ * n / m;
            if (MIN_VCO..=max_vco).contains(&vco) {
                let delta = ((vco + pl / 2) / pl).abs_diff(target);
                if delta < best_delta {
                    best_delta = delta;
                    best = Mnp { m, n, pl };
                    if delta == 0 {
                        break 'search;
                    }
                }
            }
        }
    }
    best
}

/// DFS settings for a rail voltage (`gm20b_clk_dvfs`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Dvfs {
    pub coeff: u32,
    pub ext_cal: i32,
    pub det_max: i32,
}

/// Voltage detector calibration: µV per step and µV at step 0.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Calib {
    pub slope: i32,
    pub offs: i32,
}

/// ADC slope when the chip is not fused (`gm20b_clk_init_dvfs`).
pub const ADC_SLOPE_UV: i32 = 10_000;
/// `gm20b_dvfs_params`: DFS coefficient = slope * mV + offset.
const COEFF_SLOPE: i64 = -165_230;
const COEFF_OFFS: i64 = 214_007;
/// Detection range: 2^6 steps each way; SDM_DIN range 2^12.
const DFS_DET_RANGE: u32 = 6;
const SDM_DIN_RANGE: u32 = 12;

/// Linux DIV_ROUND_CLOSEST on signed values.
fn div_round_closest(x: i64, d: i64) -> i64 {
    if (x > 0) == (d > 0) {
        (x + d / 2) / d
    } else {
        (x - d / 2) / d
    }
}

/// FUSE_RESERVED_CALIB0 (`gm20b_clk_init_fused_params`); `None` when
/// the fusing revision is 0 (the PLL calibrates itself then).
pub fn fused_calib(v: u32) -> Option<Calib> {
    if (v >> 30) & 3 == 0 {
        return None;
    }
    Some(Calib {
        slope: (((v >> 24) & 0x3F) * 1000 + ((v >> 14) & 0x3FF)) as i32,
        offs: (((v >> 4) & 0x3FF) * 1000 + (v & 0xF) * 100) as i32,
    })
}

/// Calibration from the PLL's own measurement at `uv`
/// (GPCPLL_CFG3 DFS_TESTOUT after EN_DFS_CAL).
pub fn measured_calib(uv: i32, testout: u32) -> Calib {
    Calib {
        slope: ADC_SLOPE_UV,
        offs: uv - testout as i32 * ADC_SLOPE_UV,
    }
}

/// `gm20b_dvfs_calc_det_coeff`: detection pinned to the rail voltage,
/// maximum detection 0 so the output stays under the F/V curve.
pub fn det_coeff(uv: i32, cal: Calib) -> Dvfs {
    let mv = div_round_closest(uv as i64, 1000);
    let coeff = div_round_closest(mv * COEFF_SLOPE, 1000) + COEFF_OFFS;
    // In range for any rail voltage this driver sets (< 1.29 V).
    let coeff = div_round_closest(coeff, 1000).clamp(0, 0x7F) as u32;
    Dvfs {
        coeff,
        ext_cal: div_round_closest((uv - cal.offs) as i64, cal.slope as i64) as i32,
        det_max: 0,
    }
}

/// `gm20b_dvfs_calc_ndiv`: integer NDIV and SDM_DIN (fractional part)
/// giving the effective `n_eff` at voltage `uv`.
pub fn ndiv(n_eff: u32, uv: i32, cal: Calib, d: Dvfs) -> (u32, u32) {
    let mut det = div_round_closest((uv - cal.offs) as i64, cal.slope as i64) as i32;
    det -= d.ext_cal;
    det = det.min(d.det_max) * d.coeff as i32;
    let mut n = ((n_eff << DFS_DET_RANGE) as i32 - det) as u32;
    if n as i32 <= 0 {
        n = 1 << DFS_DET_RANGE;
    }
    if n >> DFS_DET_RANGE > MAX_N {
        n = MAX_N << DFS_DET_RANGE;
    }
    let rem = n & ((1 << DFS_DET_RANGE) - 1);
    let rem = (rem << (SDM_DIN_RANGE + 1 - DFS_DET_RANGE)).wrapping_sub(1 << SDM_DIN_RANGE);
    (n >> DFS_DET_RANGE, (rem >> 8) & 0xFF)
}

/// Linux speedo-tegra210.c: GPU speedo value from CPU_SPEEDO_2 and the
/// speedo fusing revision.
pub fn gpu_speedo(rev: u32, cpu_speedo_2: u32) -> i32 {
    let v = cpu_speedo_2 as i32;
    match rev {
        3.. => v,
        2 => (-1662 + 1082 * v / 100) / 10,
        _ => v - 75,
    }
}

/// GPU speedo ID of a chip SKU (Linux `rev_sku_to_speedo_ids`, A02+):
/// the Switch's Erista is 0x83. Others are left alone.
pub fn gpu_speedo_id(sku: u32) -> Option<usize> {
    (sku == 0x83).then_some(3)
}

/// `gm20b_na_cvb_coef` up to 921.6 MHz (sys-clk's steps): c0 per clock,
/// c1-c5 shared. And `speedo_to_vmin`.
const NA_CVB_C0: [(u32, i64); 12] = [
    (76_800, 814_294),
    (153_600, 856_185),
    (230_400, 898_077),
    (307_200, 939_968),
    (384_000, 981_860),
    (460_800, 1_023_751),
    (537_600, 1_065_642),
    (614_400, 1_107_534),
    (691_200, 1_149_425),
    (768_000, 1_191_317),
    (844_800, 1_233_208),
    (921_600, 1_275_100),
];
const NA_CVB: [i64; 5] = [8_144, -940, 808, -21_583, 226];
const SPEEDO_VMIN: [u32; 5] = [950_000, 840_000, 818_750, 840_000, 810_000];
/// Added to NVIDIA's voltage for each clock.
pub const MARGIN_UV: u32 = 25_000;
/// Highest rail voltage nouveau asks the GPU regulator for.
pub const MAX_UV: u32 = 1_200_000;
/// MAX77621 VOUT code 0 and step (hekate bdk/power/max7762x.c).
pub const RAIL_BASE_UV: u32 = 606_250;
pub const RAIL_STEP_UV: u32 = 6_250;

/// `gk20a_volt_calc_voltage`: CVB at -10 C, rounded up to the mV.
fn cvb_uv(c0: i64, speedo: i32) -> u32 {
    let [c1, c2, c3, c4, c5] = NA_CVB;
    let s = speedo as i64;
    let t = -10;
    let mut mv = div_round_closest(c2 * s, 100);
    mv = div_round_closest((mv + c1) * s, 100) + c0;
    let tv = div_round_closest(c3 * s, 100) + c4 + div_round_closest(c5 * t, 10);
    let uv = div_round_closest(tv * t, 10) + mv;
    (uv.max(0) as u32).div_ceil(1000) * 1000
}

/// NVIDIA's voltage for `khz` (a step of the table): CVB, at least the
/// speedo's Vmin (`gk20a_volt_ctor`).
fn nvidia_uv(khz: u32, speedo: i32, speedo_id: usize) -> Option<u32> {
    let &(_, c0) = NA_CVB_C0.iter().find(|&&(k, _)| k == khz)?;
    Some(cvb_uv(c0, speedo).max(SPEEDO_VMIN[speedo_id]))
}

/// Rail voltage (µV, on the regulator's grid) for `khz`: NVIDIA's plus
/// `MARGIN_UV`. `None`: not a step, or above `MAX_UV`.
pub fn rail_uv(khz: u32, speedo: i32, speedo_id: usize) -> Option<u32> {
    let uv = rail_step(rail_code(nvidia_uv(khz, speedo, speedo_id)? + MARGIN_UV));
    (uv <= MAX_UV).then_some(uv)
}

/// 90% of the fastest step at the lowest voltage (`gm20b_clk_init_safe_fmax`):
/// safe at any voltage in range with DFS coefficient 0.
pub fn safe_fmax_khz(speedo: i32, speedo_id: usize) -> u32 {
    let uv = |k| nvidia_uv(k, speedo, speedo_id).unwrap_or(u32::MAX);
    let vmin = NA_CVB_C0.iter().map(|&(k, _)| uv(k)).min().unwrap_or(0);
    let fmax = NA_CVB_C0
        .iter()
        .filter(|&&(k, _)| uv(k) == vmin)
        .map(|&(k, _)| k)
        .max()
        .unwrap_or(GPC_KHZ);
    fmax * (100 - 10) / 100
}

/// PLL below both clocks of a change while the detector moves to the
/// new voltage (`gm20b_dvfs_calc_safe_pll`, in kHz throughout: nouveau
/// divides by the reference in Hz there, always taking the VCO-min
/// branch). Under VCO min: NDIV at the minimum, PL raised.
pub fn safe_pll(p: Mnp, safe_fmax_khz: u32) -> Mnp {
    let mut rate = p.khz();
    if rate > safe_fmax_khz {
        rate = rate * (100 - 10) / 100;
    }
    let rate = rate * 2;
    let n_min = (p.m * MIN_VCO).div_ceil(REF_KHZ);
    let mut s = Mnp {
        n: p.m * rate / REF_KHZ,
        ..p
    };
    if s.n < n_min {
        s.pl = (n_min * REF_KHZ).div_ceil(p.m * rate);
        s.n = n_min;
    }
    s
}

/// VOUT code for `uv`, rounded up to the grid.
pub fn rail_code(uv: u32) -> u8 {
    uv.saturating_sub(RAIL_BASE_UV).div_ceil(RAIL_STEP_UV) as u8
}

pub fn rail_step(code: u8) -> u32 {
    RAIL_BASE_UV + code as u32 * RAIL_STEP_UV
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn coefficients() {
        // VCO 1382.4 MHz / 9 / 2, the lowest VCO above 1300 MHz.
        let p = calc(GPC_KHZ);
        assert_eq!(p, Mnp { m: 1, n: 36, pl: 9 });
        assert_eq!(p.khz(), 76_800);
        assert_eq!(Mnp::from_coeff(p.coeff()), p);
        assert_eq!(p.n_lo(), 34);
        // nouveau's other pstates, from a Python port of its search.
        for (khz, n, pl) in [
            (153_600, 40, 5),
            (230_400, 36, 3),
            (307_200, 48, 3),
            (460_800, 48, 2),
            (768_000, 40, 1),
            (998_400, 52, 1),
        ] {
            assert_eq!(calc(khz), Mnp { m: 1, n, pl }, "{khz} kHz");
        }
    }

    #[test]
    fn calibration() {
        assert_eq!(fused_calib(0x1234_5678 & 0x3FFF_FFFF), None);
        // rev 1, slope 10 + 0.500 mV, intercept 600 + 0.3 mV.
        let v = 1 << 30 | 10 << 24 | 500 << 14 | 600 << 4 | 3;
        assert_eq!(
            fused_calib(v),
            Some(Calib {
                slope: 10_500,
                offs: 600_300
            })
        );
        let c = measured_calib(868_750, 27);
        assert_eq!(
            c,
            Calib {
                slope: 10_000,
                offs: 598_750
            }
        );
    }

    #[test]
    fn dvfs() {
        let cal = Calib {
            slope: 10_000,
            offs: 598_750,
        };
        let d = det_coeff(868_750, cal);
        // 869 mV * -165.23 + 214007 -> 70.4 -> 70; (868750 - 598750) / 10000.
        assert_eq!(
            d,
            Dvfs {
                coeff: 70,
                ext_cal: 27,
                det_max: 0
            }
        );
        // Detection at the calibrated voltage: no correction, SDM_DIN
        // cancels the 1/2 the hardware adds.
        assert_eq!(ndiv(36, 868_750, cal, d), (36, 0xF0));
        assert_eq!(ndiv(34, 868_750, cal, d), (34, 0xF0));
    }

    #[test]
    fn steps() {
        // Every step at its exact frequency, within the PLL's ranges.
        for &(khz, _) in NA_CVB_C0.iter() {
            let p = calc(khz);
            assert_eq!(p.khz(), khz, "{khz} kHz");
            let vco = REF_KHZ * p.n / p.m;
            assert!((MIN_VCO..=MAX_VCO).contains(&vco), "{khz} kHz: VCO {vco}");
        }
        // Above VCO min at PL 1: 90%, a straight NDIV slide.
        let safe = safe_pll(calc(768_000), 483_840);
        assert_eq!(safe, Mnp { m: 1, n: 36, pl: 1 });
        assert!(safe.khz() <= 768_000 * 9 / 10);
        // Under VCO min: NDIV at its minimum, PL raised, below the clock.
        let safe = safe_pll(calc(460_800), 483_840);
        assert_eq!((safe.n, safe.pl, safe.khz()), (34, 2, 326_400));
        let safe = safe_pll(Mnp { m: 1, n: 34, pl: 1 }, 483_840);
        assert_eq!((safe.n, safe.pl), (34, 2));
        assert!(safe.khz() <= 652_800 * 9 / 10);
    }

    #[test]
    fn voltage() {
        // Speedo ID 3 (Erista): Vmin 840 mV dominates, + 25 mV, on the grid.
        for s in [1_900, 2_000, 2_100, 2_300] {
            assert_eq!(rail_uv(GPC_KHZ, s, 3), Some(868_750), "speedo {s}");
        }
        assert!(cvb_uv(NA_CVB_C0[0].1, 2_000) < 700_000);
        // The Switch run 26's chip (speedo 2081): Vmin up to 537.6 MHz,
        // then NVIDIA's curve, 1043 mV at 921.6 MHz.
        let rails: [u32; 12] = [
            868_750, 868_750, 868_750, 868_750, 868_750, 868_750, 868_750, 900_000, 943_750,
            987_500, 1_031_250, 1_068_750,
        ];
        for (&(khz, _), uv) in NA_CVB_C0.iter().zip(rails) {
            assert_eq!(rail_uv(khz, 2_081, 3), Some(uv), "{khz} kHz");
        }
        assert_eq!(rail_uv(998_400, 2_081, 3), None);
        assert_eq!(safe_fmax_khz(2_081, 3), 483_840);
        assert_eq!(rail_code(868_750), 42);
        assert_eq!(gpu_speedo(3, 2_000), 2_000);
        assert_eq!(gpu_speedo(2, 2_000), (-1662 + 21640) / 10);
        assert_eq!(gpu_speedo(0, 2_000), 1_925);
        assert_eq!(gpu_speedo_id(0x83), Some(3));
        assert_eq!(gpu_speedo_id(0x08), None);
    }
}
