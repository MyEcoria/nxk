//! audio: the Switch's sound output. The ALC5639 codec (Linux's rt5640
//! driver, I2C1 0x1c) takes I2S1 from the Tegra X1's audio hub: a PCM
//! stream goes through an ADMAIF channel and the crossbar to I2S1, which
//! is the bit-clock and frame master; the codec's MCLK is EXTERN1
//! (12.288 MHz = 256 fs). Register sequences from the Switch's Linux
//! (CTCaer/switch-l4t-platform-t210-nx tegra210-odin DT, tegra-alt and
//! mainline sound/soc/tegra, sound/soc/codecs/rt5640.c). Clocks and pad
//! power are `clk`'s (`clk::AUDIO`).
//!
//! First stage: the CPU writes samples into the ADMAIF TX FIFO; a tone.

#![no_std]
#![no_main]

extern crate alloc;

use libnx::abi::proto::{audio as proto, clk};
use libnx::drv::{self, Mmio};
use libnx::i2c::I2c;
use libnx::ipc::Msg;
use libnx::{println, time};

const PINMUX: u64 = 0x7000_3000;
const GPIO: u64 = 0x6000_D000;
const AHUB: u64 = 0x702D_0000;

/// Pinmux registers (offsets in the pinmux page, from the DT's comments)
/// and fields (hekate bdk/soc/pinmux.h): function, pull, tristate, input.
const PM_AUD_MCLK: usize = 0x180;
const PM_DAP1_FS: usize = 0x124;
const PM_DAP1_DIN: usize = 0x128;
const PM_DAP1_DOUT: usize = 0x12C;
const PM_DAP1_SCLK: usize = 0x130;
/// pz4 (codec LDO1 enable), function "rsvd1" in the DT.
const PM_PZ4: usize = 0x28C;
const PM_PULL_DOWN: u32 = 1 << 2;
const PM_TRISTATE: u32 = 1 << 4;
const PM_INPUT: u32 = 1 << 6;

/// GPIO port Z (25), pin 4: LDO1_EN of the codec.
const PORT_Z: usize = 25;
const GPIO_CNF: usize = 0x00;
const GPIO_OE: usize = 0x10;
const GPIO_OUT: usize = 0x20;
const fn port(reg: usize, p: usize) -> usize {
    reg + (p >> 2) * 0x100 + (p & 3) * 4
}
const LDO1_EN: u32 = 1 << 4;

/// Audio hub (tegra210_{ahub,admaif,i2s}.h): crossbar at +0x800 (one
/// register per destination, a bit per source), ADMAIF at +0, TX channels
/// from 0x300 (0x40 apart), global block at 0x700, I2S1 at +0x1000.
const XBAR: usize = 0x800;
const XBAR_I2S1_RX1: usize = 0x40;
/// Crossbar source bit: ADMAIF1 (its TX path), part 0 bit 0.
const XBAR_SRC_ADMAIF1: u32 = 1;
const ADMAIF_TX1: usize = 0x300;
const ADMAIF_GLOBAL: usize = 0x700;
const ADMAIF_TX_ENABLE: usize = 0x00;
const ADMAIF_TX_STATUS: usize = 0x0C;
const ADMAIF_TX_CIF_CTRL: usize = 0x20;
const ADMAIF_TX_FIFO_CTRL: usize = 0x28;
const ADMAIF_TX_FIFO_WRITE: usize = 0x2C;
const ADMAIF_GLOBAL_ENABLE: usize = 0x00;
const ADMAIF_GLOBAL_CG: usize = 0x08;
/// Two 16-bit samples per FIFO word.
const ADMAIF_PACK16: u32 = 1 << 30;

const I2S1: usize = 0x1000;
const I2S_RX_ENABLE: usize = 0x00;
const I2S_RX_SOFT_RESET: usize = 0x04;
const I2S_RX_STATUS: usize = 0x0C;
const I2S_RX_CIF_CTRL: usize = 0x20;
const I2S_RX_CTRL: usize = 0x24;
const I2S_TX_CTRL: usize = 0x64;
const I2S_ENABLE: usize = 0x80;
const I2S_CG: usize = 0x88;
const I2S_CTRL: usize = 0xA0;
const I2S_TIMING: usize = 0xA4;
const I2S_CTRL_MASTER: u32 = 1 << 10;
const I2S_BITS_16: u32 = 3;
/// I2S data offset (bit 8 up): one bit clock after the frame edge.
const I2S_DATA_OFFSET: u32 = 1 << 8;
const I2S_FSYNC_WIDTH_31: u32 = 31 << 24;

/// Audio CIF control (tegra_cif.h): FIFO threshold 24, channels 20 and
/// 16 (minus one), bits 12 and 8 (3 = 16 bits).
const fn cif(threshold: u32) -> u32 {
    threshold << 24 | 1 << 20 | 1 << 16 | 3 << 12 | 3 << 8
}

/// Codec: I2C1, address 0x1c, 8-bit register, 16-bit value (rt5640.h).
const CODEC_BUS: u8 = 1;
const CODEC_ADDR: u8 = 0x1C;
const RT_RESET: u8 = 0x00;
const RT_SPK_VOL: u8 = 0x01;
const RT_AD_DA_MIXER: u8 = 0x29;
const RT_STO_DAC_MIXER: u8 = 0x2A;
const RT_SPK_L_MIXER: u8 = 0x46;
const RT_SPK_R_MIXER: u8 = 0x47;
const RT_SPO_L_MIXER: u8 = 0x48;
const RT_SPO_R_MIXER: u8 = 0x49;
const RT_PWR_DIG1: u8 = 0x61;
const RT_PWR_ANLG1: u8 = 0x63;
const RT_PWR_MIXER: u8 = 0x65;
const RT_PWR_VOL: u8 = 0x66;
const RT_I2S1_SDP: u8 = 0x70;
const RT_ADDA_CLK1: u8 = 0x73;
const RT_GLB_CLK: u8 = 0x80;
const RT_VENDOR_ID: u8 = 0xFD;
const RT_VENDOR_ID2: u8 = 0xFE;

struct Audio {
    i2c: I2c,
    pinmux: Mmio,
    gpio: Mmio,
    ahub: Mmio,
    ready: bool,
    codec_id: u32,
}

impl Audio {
    fn codec_read(&self, reg: u8) -> Result<u16, i64> {
        let mut v = [0u8; 2];
        self.i2c.write_read(CODEC_BUS, CODEC_ADDR, &[reg], &mut v)?;
        Ok(u16::from_be_bytes(v))
    }

    fn codec_write(&self, reg: u8, val: u16) -> Result<(), i64> {
        let v = val.to_be_bytes();
        self.i2c
            .transfer(CODEC_BUS, CODEC_ADDR, &[reg, v[0], v[1]], &mut [])
    }

    fn codec_update(&self, reg: u8, mask: u16, val: u16) -> Result<(), i64> {
        let old = self.codec_read(reg)?;
        self.codec_write(reg, old & !mask | val & mask)
    }

    fn pads(&self) {
        let pm = &self.pinmux;
        // MCLK (function AUD), I2S1's clocks and data out: outputs; data
        // in an input (as the DT).
        pm.w32(PM_AUD_MCLK, PM_PULL_DOWN);
        for p in [PM_DAP1_FS, PM_DAP1_DOUT, PM_DAP1_SCLK] {
            pm.w32(p, PM_PULL_DOWN);
        }
        pm.w32(PM_DAP1_DIN, PM_PULL_DOWN | PM_TRISTATE | PM_INPUT);
        // Codec LDO1 enable: pz4 as a GPIO output, high.
        pm.w32(PM_PZ4, PM_PULL_DOWN | 1);
        let g = &self.gpio;
        g.modify(port(GPIO_CNF, PORT_Z), 0, LDO1_EN);
        g.modify(port(GPIO_OUT, PORT_Z), 0, LDO1_EN);
        g.modify(port(GPIO_OE, PORT_Z), 0, LDO1_EN);
        let _ = g.r32(port(GPIO_OE, PORT_Z));
    }

    /// The audio hub for a 16-bit stereo 48 kHz stream: ADMAIF channel 1
    /// to I2S1 through the crossbar, I2S1 the bit-clock master (LRCK
    /// mode, I2S format, 16 bits), not yet enabled.
    fn hub(&self) {
        let a = &self.ahub;
        // ADMAIF: clock gating off, global enable, channel 1 packed 16-bit.
        a.w32(ADMAIF_GLOBAL + ADMAIF_GLOBAL_CG, 3);
        a.w32(ADMAIF_GLOBAL + ADMAIF_GLOBAL_ENABLE, 1);
        a.w32(ADMAIF_TX1 + ADMAIF_TX_CIF_CTRL, ADMAIF_PACK16 | cif(0));
        // Crossbar: I2S1 takes ADMAIF1.
        a.w32(XBAR + XBAR_I2S1_RX1, XBAR_SRC_ADMAIF1);
        // I2S1.
        let i = I2S1;
        a.w32(
            i + I2S_CTRL,
            I2S_CTRL_MASTER | I2S_BITS_16 | I2S_FSYNC_WIDTH_31,
        );
        a.w32(i + I2S_RX_CTRL, I2S_DATA_OFFSET);
        a.w32(i + I2S_TX_CTRL, I2S_DATA_OFFSET);
        // Bit count per channel - 1: 16 bit clocks.
        a.w32(i + I2S_TIMING, 15);
        a.w32(i + I2S_RX_CIF_CTRL, cif(3));
        a.w32(i + I2S_CG, 1);
    }

    /// Codec after reset, for the speakers: DAC from I2S1, slave on
    /// MCLK = 256 fs, the mixers on the Speaker path unmuted.
    fn codec(&self) -> Result<(), i64> {
        self.codec_write(RT_RESET, 0)?;
        time::sleep_ms(20);
        let (v1, v2) = (
            self.codec_read(RT_VENDOR_ID)?,
            self.codec_read(RT_VENDOR_ID2)?,
        );
        self.codec_id_report(v1, v2);
        // References first, then the fast ones (rt5640_vref_event).
        const VREF1: u16 = 1 << 15;
        const VREF2: u16 = 1 << 4;
        const FV1: u16 = 1 << 14;
        const FV2: u16 = 1 << 3;
        const BG: u16 = 1 << 11;
        const LDO2: u16 = 1 << 2;
        self.codec_write(RT_PWR_ANLG1, VREF1 | VREF2 | BG | LDO2)?;
        time::sleep_ms(20);
        self.codec_update(RT_PWR_ANLG1, FV1 | FV2, FV1 | FV2)?;
        // I2S1, DAC L1/R1, class D amplifier driver.
        self.codec_write(RT_PWR_DIG1, 1 << 15 | 1 << 12 | 1 << 11 | 1)?;
        // Output and speaker mixers, speaker volume stages.
        self.codec_write(RT_PWR_MIXER, 1 << 15 | 1 << 14 | 1 << 13 | 1 << 12)?;
        self.codec_write(RT_PWR_VOL, 1 << 15 | 1 << 14)?;
        // Slave, I2S, 16 bits; MCLK as the system clock; MCLK is 256 fs:
        // no extra divider.
        self.codec_write(RT_I2S1_SDP, 1 << 15)?;
        self.codec_write(RT_GLB_CLK, 0)?;
        self.codec_update(RT_ADDA_CLK1, 7 << 12, 0)?;
        // Path: INF1 -> DAC MIX -> Stereo DAC MIX -> SPK MIX -> SPKVOL ->
        // SPO MIX. Only these switches are opened.
        self.codec_update(RT_AD_DA_MIXER, 1 << 14 | 1 << 6, 0)?;
        self.codec_update(RT_STO_DAC_MIXER, 1 << 14 | 1 << 6, 0)?;
        self.codec_update(RT_SPK_L_MIXER, 1 << 3, 0)?;
        self.codec_update(RT_SPK_R_MIXER, 1 << 3, 0)?;
        self.codec_update(RT_SPO_L_MIXER, 1 << 12, 0)?;
        self.codec_update(RT_SPO_R_MIXER, 1 << 12, 0)?;
        // Speaker volume: unmuted both ways, 12 steps (1.5 dB) below full.
        self.codec_write(RT_SPK_VOL, 0x0C0C)?;
        Ok(())
    }

    fn codec_id_report(&self, v1: u16, v2: u16) {
        println!("[audio] codec ID {:#06x} {:#06x}", v1, v2);
    }

    /// Start the stream: I2S1 on (the codec gets its clocks), then the
    /// ADMAIF channel.
    fn start(&self) {
        let a = &self.ahub;
        let i = I2S1;
        a.w32(i + I2S_RX_SOFT_RESET, 1);
        time::poll(10_000, || a.r32(i + I2S_RX_SOFT_RESET) & 1 == 0);
        // Reset keeps the control registers (Linux restores them anyway).
        a.w32(i + I2S_ENABLE, 1);
        a.w32(i + I2S_RX_ENABLE, 1);
        a.w32(ADMAIF_TX1 + ADMAIF_TX_ENABLE, 1);
    }

    fn stop(&self) {
        let a = &self.ahub;
        a.w32(ADMAIF_TX1 + ADMAIF_TX_ENABLE, 0);
        a.w32(I2S1 + I2S_RX_ENABLE, 0);
    }

    fn dump(&self, what: &str) {
        let a = &self.ahub;
        println!(
            "[audio] {}: I2S1 en {:#x} rx {:#x} status {:#x} ctrl {:#x} timing {:#x}; ADMAIF1 en {:#x} status {:#x} fifo {:#x} cif {:#x}; xbar {:#x}",
            what,
            a.r32(I2S1 + I2S_ENABLE),
            a.r32(I2S1 + I2S_RX_ENABLE),
            a.r32(I2S1 + I2S_RX_STATUS),
            a.r32(I2S1 + I2S_CTRL),
            a.r32(I2S1 + I2S_TIMING),
            a.r32(ADMAIF_TX1 + ADMAIF_TX_ENABLE),
            a.r32(ADMAIF_TX1 + ADMAIF_TX_STATUS),
            a.r32(ADMAIF_TX1 + ADMAIF_TX_FIFO_CTRL),
            a.r32(ADMAIF_TX1 + ADMAIF_TX_CIF_CTRL),
            a.r32(XBAR + XBAR_I2S1_RX1),
        );
    }

    fn up(&mut self) -> Result<(), i64> {
        libnx::clk::enable(clk::I2C1).inspect_err(|e| println!("[audio] I2C1 clock: {}", e))?;
        libnx::clk::enable(clk::AUDIO).inspect_err(|e| println!("[audio] clk AUDIO: {}", e))?;
        self.pads();
        time::sleep_ms(10);
        self.codec()
            .inspect_err(|e| println!("[audio] codec: {}", e))?;
        self.hub();
        self.dump("configured");
        self.ready = true;
        Ok(())
    }

    /// A sine of `hz` for `ms`, written to the FIFO a few frames at a
    /// time as the time passes. Returns the frames written.
    fn tone(&self, hz: u32, ms: u32, level: u32) -> u64 {
        let a = &self.ahub;
        let fifo = ADMAIF_TX1 + ADMAIF_TX_FIFO_WRITE;
        let amp = (level.min(100) * 300) as i32;
        let total = ms as u64 * proto::RATE as u64 / 1000;
        let (mut done, mut phase) = (0u64, 0u32);
        // Prefill, then start: the FIFO should not run dry at once.
        let step = ((hz as u64) << 16) / proto::RATE as u64;
        let frame = |phase: &mut u32| -> u32 {
            let p = *phase & 0xFFFF;
            *phase = phase.wrapping_add(step as u32);
            // Parabolic sine: t (32768 - t) peaks at 2^28.
            let half = p & 0x7FFF;
            let v = ((half * (32768 - half)) >> 13) as i32; // 0..32768
            let s = if p & 0x8000 == 0 { v } else { -v };
            let s = (s * amp / 32768) as i16 as u16 as u32;
            s | s << 16
        };
        for _ in 0..16 {
            a.w32(fifo, frame(&mut phase));
            done += 1;
        }
        self.start();
        self.dump("running");
        let t0 = time::now_ns();
        while done < total {
            let due = (time::now_ns() - t0) * proto::RATE as u64 / 1_000_000_000 + 24;
            let n = due.saturating_sub(done).min(total - done).min(24);
            for _ in 0..n {
                a.w32(fifo, frame(&mut phase));
            }
            done += n;
            if n == 0 {
                time::sleep_us(150);
            }
        }
        self.dump("done");
        self.stop();
        done
    }
}

libnx::entry!(main);

fn main() -> i32 {
    let (Ok(pinmux), Ok(gpio), Ok(ahub)) = (
        Mmio::map(drv::dev_slot(0), PINMUX, 0x1000),
        Mmio::map(drv::dev_slot(1), GPIO, 0x1000),
        Mmio::map(drv::dev_slot(2), AHUB, 0x2000),
    ) else {
        println!("[audio] no register access");
        return 1;
    };
    let Ok(i2c) = I2c::connect() else {
        println!("[audio] no i2c service");
        return 1;
    };
    let mut audio = Audio {
        i2c,
        pinmux,
        gpio,
        ahub,
        ready: false,
        codec_id: 0,
    };
    // Powering up stays off the boot path: the hardware is touched by the
    // first request (`STATUS` or `TONE`).
    let ep = drv::publish("audio").expect("audio: publish");
    println!("[audio] service ready (off)");
    drv::serve(ep, |rx| {
        let m = &rx.msg;
        if !audio.ready {
            if let Err(e) = audio.up() {
                println!("[audio] power-up failed: error {}", e);
                return Some(Msg::error(e));
            }
        }
        match m.label() {
            proto::TONE => {
                let frames = audio.tone(m.mr[0] as u32, m.mr[1] as u32, m.mr[2] as u32);
                Some(Msg::from_words(0, &[frames]))
            }
            proto::STATUS => {
                audio.dump("status");
                Some(Msg::from_words(
                    0,
                    &[
                        if audio.ready { proto::ST_READY } else { 0 },
                        audio.codec_id as u64,
                    ],
                ))
            }
            _ => Some(Msg::error(-nxk_abi_einval())),
        }
    })
}

fn nxk_abi_einval() -> i64 {
    libnx::abi::err::EINVAL
}
