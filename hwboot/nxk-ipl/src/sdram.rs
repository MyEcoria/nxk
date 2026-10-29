//! DRAM init (hekate bdk/mem/sdram.c): the parameters of the board's DRAM
//! (hekate's base table for the SoC, patched for the fused DRAM id), the
//! DRAM rail, then the configuration sequence (`sdram_seq`).

use crate::hdr::*;
use crate::i2c::{self, I2C_5};
use crate::pmic;
use crate::sdram_seq::{config_t210, config_t210b01};
use crate::sdram_tables::*;
use crate::soc;

/// Where hekate builds the parameters (IRAM): the LP0 save reads them there.
const SDRAM_PARAMS_ADDR: u32 = 0x4003_0000;

/// `fuse_read_dramid(false)`: the fused DRAM id, out-of-range ids mapped
/// to the SoC's default (T210: 0, T210B01: 8).
pub fn dram_id() -> u32 {
    let t210 = soc::is_t210();
    let odm4 = soc::fuse(FUSE_RESERVED_ODM4);
    let mut id = (odm4 & 0xF8) >> 3;
    if !t210 {
        id |= (odm4 & 0x7000) >> 7;
    }
    if t210 && id > 7 {
        0
    } else if !t210 && id > 34 {
        8
    } else {
        id
    }
}

/// The base table copied to `SDRAM_PARAMS_ADDR`, then the patches whose
/// mask has bit `sel`.
fn build(base: &[u32], patches: &[(u32, u32, u16)], sel: Option<u32>) -> *mut u32 {
    let dst = SDRAM_PARAMS_ADDR as *mut u32;
    unsafe {
        core::ptr::copy_nonoverlapping(base.as_ptr(), dst, base.len());
        if let Some(sel) = sel {
            for &(val, mask, off) in patches {
                if mask & 1 << sel != 0 {
                    dst.add(off as usize).write(val);
                }
            }
        }
    }
    dst
}

/// `_sdram_get_params_t210`.
pub fn params_t210() -> &'static ParamsT210 {
    let p = build(&PARAMST210_BASE, &PARAMST210_PATCHES, Some(dram_id()));
    unsafe { &*(p as *const ParamsT210) }
}

/// `sdram_get_params_t210b01`: patched by the id's code (none for code 0).
pub fn params_t210b01() -> &'static ParamsT210b01 {
    let code = DRAM_ENCODING_T210B01[dram_id() as usize] as u32;
    let p = build(&PARAMST210B01_BASE, &PARAMST210B01_PATCHES, (code != 0).then_some(code));
    unsafe { &*(p as *const ParamsT210b01) }
}

/// `sdram_init`.
pub fn init() {
    // Remote sense off for SD1 (the DRAM rail).
    let _ = i2c::send_byte(
        I2C_5,
        MAX77620_I2C_ADDR,
        MAX77620_REG_SD_CFG2,
        (MAX77620_SD_CNF2_ROVS_EN_SD0 | MAX77620_SD_CNF2_RSVD) as u8,
    );
    if soc::is_t210() {
        let p = params_t210();
        if p.memory_type != MEMORY_TYPE_LPDDR4 {
            return;
        }
        // HOS: 1.125 V; hekate's bootloader too.
        pmic::set_voltage(REGULATOR_SD1, 1_125_000);
        config_t210(p);
    } else {
        let p = params_t210b01();
        if p.memory_type != MEMORY_TYPE_LPDDR4 {
            return;
        }
        config_t210b01(p);
    }
}
