//! The panel, lit as hekate does (bdk/display/di.c `display_init`,
//! `display_init_window_a_pitch`, the backlight), for every Switch panel:
//! the Erista and Mariko 6.2" LCDs, the Lite's 5.5" ones and the OLED.
//! Window A then scans hekate's framebuffer (720x1280 portrait,
//! B8G8R8A8 at 0xF5A00000), where nxk-ipl draws the boot screen and where
//! nxk-bl31 and the kernel find it.

use nxk_bootsplash::{self as splash, Format, Splash};

use crate::clock::{self, car, set_car, ClkRst};
use crate::di_tables::*;
use crate::hdr::*;
use crate::pmic;
use crate::reg::{rd, wr};
use crate::soc::{self, gpio_config, gpio_direction_output, gpio_write, pinmux, pinmux_rd};
use crate::timer::{now_us, usleep};

pub const FB: u32 = IPL_FB_ADDRESS;
const FB_W: u32 = 720;
const FB_H: u32 = 1280;

/// The panel's decoded id (`_panel_id`), 0 before `init`.
static mut PANEL_ID: u32 = 0;

fn panel_id() -> u32 {
    unsafe { PANEL_ID }
}

fn dc(idx: u32) -> u32 {
    DISPLAY_A_BASE + (idx << 2)
}
fn dsi(idx: u32) -> u32 {
    DSI_BASE + (idx << 2)
}
fn mipi_cal(idx: u32) -> u32 {
    MIPI_CAL_BASE + (idx << 2)
}

/// `reg_write_array`: (word index, value) pairs from `base`.
fn write_table(base: u32, t: &[(u32, u32)]) {
    for &(i, v) in t {
        wr(base + (i << 2), v);
    }
}

/// `_display_dsi_wait`: until `mask` clears at DSI `off`, `timeout` ms at most.
fn dsi_wait(timeout_ms: u32, off: u32, mask: u32) {
    let end = now_us().wrapping_add(timeout_ms * 1000);
    while now_us() < end && rd(dsi(off)) & mask != 0 {}
    usleep(5);
}

fn dsi_send_cmd(cmd: u32, param: u32, wait_us: u32) {
    wr(dsi(DSI_WR_DATA), param << 8 | cmd);
    wr(dsi(DSI_TRIGGER), DSI_TRIGGER_HOST);
    if wait_us != 0 {
        usleep(wait_us);
    }
}

/// `_display_dsi_read_rx_fifo`: the RX FIFO into `data`, or drained.
fn dsi_read_rx_fifo(mut data: Option<&mut [u32; 32]>) {
    let count = rd(dsi(DSI_STATUS)) & DSI_STATUS_RX_FIFO_SIZE;
    if count != 0 {
        wr(dsi(DSI_TRIGGER), 0);
    }
    for i in 0..count as usize {
        let v = rd(dsi(DSI_RD_DATA));
        if let Some(d) = data.as_deref_mut() {
            d[i] = v;
        }
    }
}

/// `display_dsi_read`: a DCS read of `len` bytes; None if no valid reply.
fn dsi_read(cmd: u32, len: u32) -> Option<u32> {
    let mut fifo = [0u32; 32];
    dsi_read_rx_fifo(None);
    dsi_send_cmd(MIPI_DSI_SET_MAXIMUM_RETURN_PACKET_SIZE, len, 0);
    dsi_wait(250, DSI_TRIGGER, DSI_TRIGGER_HOST | DSI_TRIGGER_VIDEO);
    dsi_send_cmd(MIPI_DSI_DCS_READ, cmd, 0);
    dsi_wait(250, DSI_TRIGGER, DSI_TRIGGER_HOST | DSI_TRIGGER_VIDEO);
    wr(dsi(DSI_HOST_CONTROL), rd(dsi(DSI_HOST_CONTROL)) | DSI_HOST_CONTROL_IMM_BTA);
    dsi_wait(150, DSI_HOST_CONTROL, DSI_HOST_CONTROL_IMM_BTA);
    usleep(5000);
    dsi_read_rx_fifo(Some(&mut fifo));
    if fifo[0] & 0xFF != DSI_ESCAPE_CMD {
        return None;
    }
    let bytes = match fifo[1] & 0xFF {
        GEN_LONG_RD_RES | DCS_LONG_RD_RES => ((fifo[1] >> 8) & 0xFFFF).min(len),
        GEN_1_BYTE_SHORT_RD_RES | DCS_1_BYTE_SHORT_RD_RES => 1,
        GEN_2_BYTE_SHORT_RD_RES | DCS_2_BYTE_SHORT_RD_RES => 2,
        _ => return None,
    };
    let mask = if bytes >= 4 { u32::MAX } else { (1 << (8 * bytes)) - 1 };
    Some(fifo[2] & mask)
}

/// `clock_enable_plld`.
fn enable_plld(divp: u32, divn: u32, lowpower: bool, t210: bool) {
    let div = divp << 20 | divn << 11 | 1;
    let din = if lowpower && t210 { 0x0AAA } else { 0xFC00 };
    let misc = 1 << 21 | 1 << 19 | 1 << 18 | 1 << 16 | din;
    set_car(CLK_RST_CONTROLLER_CLK_SOURCE_DISP1, 2 << 29);
    set_car(CLK_RST_CONTROLLER_PLLD_BASE, PLL_BASE_ENABLE | PLL_BASE_LOCK | div);
    set_car(CLK_RST_CONTROLLER_PLLD_MISC1, if t210 { 0x20 } else { 0 });
    set_car(CLK_RST_CONTROLLER_PLLD_MISC, misc);
    for _ in 0..1000 {
        if car(CLK_RST_CONTROLLER_PLLD_BASE) & PLL_BASE_LOCK != 0 {
            break;
        }
        usleep(1);
    }
    usleep(2);
}

/// `display_init`, from a display path hekate never touched (RCM).
pub fn init() {
    let aula = soc::hw_type() == soc::HW_AULA;
    let t210 = soc::is_t210();

    // DSI AVDD.
    pmic::set_voltage(REGULATOR_LDO0, 1_200_000);
    pmic::enable(REGULATOR_LDO0, true);

    set_car(CLK_RST_CONTROLLER_RST_DEV_H_CLR, 1 << CLK_H_MIPI_CAL | 1 << CLK_H_DSI);
    set_car(CLK_RST_CONTROLLER_CLK_ENB_H_SET, 1 << CLK_H_MIPI_CAL | 1 << CLK_H_DSI);
    set_car(CLK_RST_CONTROLLER_RST_DEV_L_CLR, 1 << CLK_L_DISP1);
    set_car(CLK_RST_CONTROLLER_CLK_ENB_L_SET, 1 << CLK_L_DISP1);
    set_car(CLK_RST_CONTROLLER_CLK_ENB_X_SET, 1 << CLK_X_UART_FST_MIPI_CAL);
    set_car(CLK_RST_CONTROLLER_CLK_SOURCE_UART_FST_MIPI_CAL, clock::src_div(6));
    set_car(CLK_RST_CONTROLLER_CLK_ENB_W_SET, 1 << CLK_W_DSIA_LP);
    set_car(CLK_RST_CONTROLLER_CLK_SOURCE_DSIA_LP, clock::src_div(6));

    wr(PMC_BASE + APBDEV_PMC_IO_DPD_REQ, PMC_IO_DPD_REQ_DPD_OFF);
    wr(PMC_BASE + APBDEV_PMC_IO_DPD2_REQ, PMC_IO_DPD_REQ_DPD_OFF);

    if !aula {
        // LCD driver and WLED (backlight) driver pins; +-5.4 V AVDD on.
        pinmux(PINMUX_AUX_NFC_EN, PINMUX_PULL_DOWN);
        pinmux(PINMUX_AUX_NFC_INT, PINMUX_PULL_DOWN);
        pinmux(PINMUX_AUX_LCD_BL_PWM, PINMUX_PULL_DOWN);
        pinmux(PINMUX_AUX_LCD_BL_EN, PINMUX_PULL_DOWN);
        gpio_direction_output(GPIO_PORT_I, GPIO_PIN_0 | GPIO_PIN_1, true);
        usleep(10_000);
        gpio_direction_output(GPIO_PORT_V, GPIO_PIN_0 | GPIO_PIN_1, false);
        gpio_write(GPIO_PORT_V, GPIO_PIN_1, true);
    }

    // Panel reset held low.
    pinmux(PINMUX_AUX_LCD_RST, PINMUX_PULL_DOWN);
    gpio_direction_output(GPIO_PORT_V, GPIO_PIN_2, false);

    wr(mipi_cal(MIPI_CAL_MIPI_BIAS_PAD_CFG2), 0);
    wr(mipi_cal(MIPI_CAL_MIPI_BIAS_PAD_CFG0), 0);
    if !t210 {
        wr(APB_MISC_BASE + APB_MISC_GP_DSI_PAD_CONTROL, 0);
    }

    // Command mode: PLLD_OUT0 about 50 MHz.
    enable_plld(3, 20, true, t210);
    write_table(DISPLAY_A_BASE, &DI_DC_INIT_CONFIG);
    write_table(DSI_BASE, &DI_DSI_SEQ_PKT_RESET_CONFIG0);
    wr(dsi(if t210 { DSI_INIT_SEQ_DATA_15 } else { DSI_INIT_SEQ_DATA_15_B01 }), 0);
    write_table(DSI_BASE, &DI_DSI_SEQ_PKT_RESET_CONFIG1);
    if !t210 {
        write_table(DSI_BASE, &DI_DSI_INIT_PADS_T210B01);
    }
    write_table(DSI_BASE, &DI_DSI_INIT_CONFIG);
    usleep(10);
    // DSI soft reset.
    wr(dsi(DSI_POWER_CONTROL), 0);
    usleep(10);
    write_table(DSI_BASE, &DI_DSI_TIMING_LP_CONFIG);
    usleep(10_000);

    gpio_write(GPIO_PORT_V, GPIO_PIN_2, true);
    usleep(60_000);
    wr(dsi(DSI_BTA_TIMING), if aula { 0x40103 } else { 0x50204 });

    let mut raw = 0xCC_CCCC;
    for _ in 0..3 {
        if let Some(v) = dsi_read(MIPI_DCS_GET_DISPLAY_ID, 3) {
            raw = v;
            break;
        }
        usleep(5000);
    }
    let mut id = (raw >> 8) & 0xFF00 | raw & 0xFF;
    if id & 0xFF == PANEL_JDI_XXX062M {
        id = PANEL_JDI_XXX062M;
    }
    if aula {
        id = PANEL_SAM_AMS699VC01;
    }
    unsafe { PANEL_ID = id };

    match id {
        PANEL_SAM_AMS699VC01 => {
            dsi_send_cmd(MIPI_DSI_DCS_SHORT_WRITE, MIPI_DCS_EXIT_SLEEP_MODE, 180_000);
            // Color mode basic (natural); brightness control and dimming.
            dsi_send_cmd(
                MIPI_DSI_DCS_SHORT_WRITE_PARAM,
                MIPI_DCS_PRIV_SM_SET_COLOR_MODE | DCS_SM_COLOR_MODE_BASIC << 8,
                0,
            );
            dsi_send_cmd(
                MIPI_DSI_DCS_SHORT_WRITE_PARAM,
                MIPI_DCS_SET_CONTROL_DISPLAY
                    | (DCS_CONTROL_DISPLAY_BRIGHTNESS_CTRL | DCS_CONTROL_DISPLAY_DIMMING_CTRL) << 8,
                0,
            );
            // Level 2 registers unlocked, PWM transition 6 frames, relocked.
            wr(dsi(DSI_WR_DATA), 0x539);
            wr(dsi(DSI_WR_DATA), 0x5A5A_5AE2);
            wr(dsi(DSI_WR_DATA), 0x5A);
            wr(dsi(DSI_TRIGGER), DSI_TRIGGER_HOST);
            dsi_send_cmd(MIPI_DSI_DCS_SHORT_WRITE_PARAM, MIPI_DCS_PRIV_SM_SET_REG_OFFSET | 7 << 8, 0);
            dsi_send_cmd(MIPI_DSI_DCS_SHORT_WRITE_PARAM, MIPI_DCS_PRIV_SM_SET_ELVSS | 6 << 8, 0);
            wr(dsi(DSI_WR_DATA), 0x539);
            wr(dsi(DSI_WR_DATA), 0xA55A_5AE2);
            wr(dsi(DSI_WR_DATA), 0xA5);
            wr(dsi(DSI_TRIGGER), DSI_TRIGGER_HOST);
            // Brightness 0 %.
            wr(dsi(DSI_WR_DATA), 0x339);
            wr(dsi(DSI_WR_DATA), 0x000051);
            wr(dsi(DSI_TRIGGER), DSI_TRIGGER_HOST);
            usleep(5000);
        }
        PANEL_JDI_XXX062M => {
            write_table(DSI_BASE, &DI_DSI_PANEL_INIT_CONFIG_JDI);
            dsi_send_cmd(MIPI_DSI_DCS_SHORT_WRITE, MIPI_DCS_EXIT_SLEEP_MODE, 180_000);
        }
        PANEL_INL_P062CCA_AZ1 | PANEL_AUO_A062TAN01 => {
            dsi_send_cmd(MIPI_DSI_DCS_SHORT_WRITE, MIPI_DCS_EXIT_SLEEP_MODE, 180_000);
            // Extension commands unlocked (FF 83 94), then power control.
            wr(dsi(DSI_WR_DATA), 0x439);
            wr(dsi(DSI_WR_DATA), 0x9483_FFB9);
            wr(dsi(DSI_TRIGGER), DSI_TRIGGER_HOST);
            usleep(5000);
            wr(dsi(DSI_WR_DATA), 0x739);
            wr(
                dsi(DSI_WR_DATA),
                if id == PANEL_INL_P062CCA_AZ1 { 0x7515_48B1 } else { 0x7111_48B1 },
            );
            wr(dsi(DSI_WR_DATA), 0x14_3209);
            wr(dsi(DSI_TRIGGER), DSI_TRIGGER_HOST);
            usleep(5000);
        }
        // Lite panels, and spare parts.
        _ => dsi_send_cmd(MIPI_DSI_DCS_SHORT_WRITE, MIPI_DCS_EXIT_SLEEP_MODE, 120_000),
    }

    dsi_send_cmd(MIPI_DSI_DCS_SHORT_WRITE, MIPI_DCS_SET_DISPLAY_ON, 20_000);

    // Video mode: PLLD_OUT0 234 MHz, pixel clock 78 MHz (60 Hz).
    enable_plld(1, 24, false, t210);
    write_table(DSI_BASE, &DI_DSI_SEQ_PKT_VIDEO_NON_BURST_NO_EOT_CONFIG);
    wr(dc(DC_DISP_DISP_CLOCK_CONTROL), PIXEL_CLK_DIVIDER_PCD1 | 4);
    write_table(DSI_BASE, &DI_DSI_HOST_MODE_CONFIG);
    usleep(10_000);

    // Pad calibration, twice (a T210B01 clock-lane erratum).
    write_table(MIPI_CAL_BASE, &DI_MIPI_PAD_CAL_CONFIG);
    for _ in 0..2 {
        wr(mipi_cal(MIPI_CAL_MIPI_BIAS_PAD_CFG2), 0x10010);
        wr(mipi_cal(MIPI_CAL_MIPI_BIAS_PAD_CFG1), if t210 { 0x300 } else { 0 });
        if t210 {
            write_table(DSI_BASE, &DI_DSI_PAD_CAL_CONFIG_T210);
            write_table(MIPI_CAL_BASE, &DI_MIPI_DSI_CAL_PROD_CONFIG_T210);
        } else {
            write_table(DSI_BASE, &DI_DSI_PAD_CAL_CONFIG_T210B01);
            write_table(MIPI_CAL_BASE, &DI_MIPI_DSI_CAL_PROD_CONFIG_T210B01);
        }
        write_table(MIPI_CAL_BASE, &DI_MIPI_DSI_CAL_UNUSED_CONFIG);
        wr(mipi_cal(MIPI_CAL_MIPI_CAL_CTRL), 0x2A00_0001);
    }
    usleep(10_000);
    write_table(DISPLAY_A_BASE, &DI_DC_VIDEO_MODE_CONFIG);
}

/// `display_init_window_a_pitch`: the framebuffer cleared, window A on it.
pub fn window_a() {
    unsafe { core::ptr::write_bytes(FB as *mut u8, 0, (FB_W * FB_H * 4) as usize) };
    write_table(DISPLAY_A_BASE, &DI_WINA_PITCH);
}

const PWM_CLK: ClkRst = ClkRst {
    reset: CLK_RST_CONTROLLER_RST_DEV_L_SET,
    enable: CLK_RST_CONTROLLER_CLK_ENB_L_SET,
    source: CLK_RST_CONTROLLER_CLK_SOURCE_PWM,
    index: CLK_L_PWM,
    clk_src: 6,
    clk_div: clock::src_div(3),
};

/// `display_backlight_pwm_init`: PWM0 at 25 kHz on the backlight pin.
pub fn backlight_pwm_init() {
    if panel_id() == PANEL_SAM_AMS699VC01 {
        return;
    }
    soc::bpmp_clk_rate_relaxed(true);
    clock::enable(&PWM_CLK);
    soc::bpmp_clk_rate_relaxed(false);
    wr(PWM_BASE + PWM_CONTROLLER_PWM_CSR_0, PWM_CSR_EN);
    pinmux(PINMUX_AUX_LCD_BL_PWM, pinmux_rd(PINMUX_AUX_LCD_BL_PWM) & !PINMUX_FUNC_MASK | 1);
    usleep(2);
    gpio_config(GPIO_PORT_V, GPIO_PIN_0, false);
}

/// The OLED's brightness through DCS (`_display_dsi_backlight_brightness`),
/// sent in the vertical blank (`display_dsi_vblank_write`).
fn dsi_brightness(duty: u32) {
    if rd(dc(DC_DCS_BACKLIGHT_LEVEL)) == duty {
        return;
    }
    let candela = duty * PANEL_SM_BL_CANDELA_MAX / 255;
    let level = (candela as u16).swap_bytes();
    // Vblank: the host packet on the 4th line, after a frame end.
    wr(dc(DC_CMD_INT_ENABLE), rd(dc(DC_CMD_INT_ENABLE)) | DC_CMD_INT_FRAME_END_INT);
    wr(dsi(DSI_VIDEO_MODE_CONTROL), DSI_CMD_PKT_VID_ENABLE | 4 << 1); // DSI_DSI_LINE_TYPE(4)
    wait_frame_end();
    // Long write: length 3 (command + 2 bytes), then the bytes.
    wr(dsi(DSI_WR_DATA), 3 << 8 | MIPI_DSI_DCS_LONG_WRITE);
    wr(dsi(DSI_WR_DATA), MIPI_DCS_SET_BRIGHTNESS | (level as u32) << 8);
    wait_frame_end();
    usleep(14);
    wr(dsi(DSI_INCR_SYNCPT_CNTRL), DSI_INCR_SYNCPT_SOFT_RESET);
    usleep(300);
    wr(dsi(DSI_INCR_SYNCPT_CNTRL), 0);
    usleep(300);
    wr(dsi(DSI_VIDEO_MODE_CONTROL), 0);
    wr(dc(DC_CMD_INT_ENABLE), rd(dc(DC_CMD_INT_ENABLE)) & !DC_CMD_INT_FRAME_END_INT);
    wr(dc(DC_CMD_INT_STATUS), DC_CMD_INT_FRAME_END_INT);
    if duty == 0 {
        usleep(100_000);
    }
    wr(dc(DC_DCS_BACKLIGHT_LEVEL), duty);
}

/// `display_wait_interrupt(DC_CMD_INT_FRAME_END_INT)`, bounded (a frame
/// is 16.7 ms).
fn wait_frame_end() {
    wr(dc(DC_CMD_INT_STATUS), DC_CMD_INT_FRAME_END_INT);
    let end = now_us().wrapping_add(50_000);
    while rd(dc(DC_CMD_INT_STATUS)) & DC_CMD_INT_FRAME_END_INT == 0 && now_us() < end {}
}

/// `display_backlight_brightness(brightness, step_delay)`.
pub fn brightness(level: u32, step_us: u32) {
    let level = level.min(255);
    if panel_id() == PANEL_SAM_AMS699VC01 {
        return dsi_brightness(level);
    }
    let reg = PWM_BASE + PWM_CONTROLLER_PWM_CSR_0;
    let old = (rd(reg) >> 16) & 0xFF;
    let mut v = old;
    while v != level {
        v = if v < level { v + 1 } else { v - 1 };
        wr(reg, PWM_CSR_EN | v << 16);
        usleep(step_us);
    }
}

/// A rectangle of the landscape view in `rgb` (0xRRGGBB), on window A.
pub fn rect(x: u32, y: u32, w: u32, h: u32, rgb: u32) {
    let (x, y, w, h) = splash::turn(x, y, w, h, FB_H);
    let v = Format::Bgra.pixel(nxk_bootsplash::rgb(rgb));
    for row in y..y + h {
        for col in x..x + w {
            wr(FB + (row * FB_W + col) * 4, v);
        }
    }
}

/// The boot screen at `p` thousandths into the framebuffer (window A).
pub fn splash(p: u32, full: bool) {
    let view = Splash::new(FB_H, FB_W);
    let mut fill = |x, y, w, h, c| {
        let (x, y, w, h) = splash::turn(x, y, w, h, FB_H);
        let v = Format::Bgra.pixel(c);
        for row in y..y + h {
            for col in x..x + w {
                wr(FB + (row * FB_W + col) * 4, v);
            }
        }
    };
    if full {
        view.draw(p, &mut fill);
    } else {
        view.bar(p, &mut fill);
    }
}
