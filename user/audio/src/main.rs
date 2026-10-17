//! audio: the Switch's sound output. The ALC5639 codec (Linux's rt5640
//! driver, I2C1 0x1c) takes I2S1 from the Tegra X1's audio hub: a PCM
//! stream goes through an ADMAIF channel and the crossbar to I2S1, which
//! is the bit-clock and frame master; the codec's MCLK is EXTERN1
//! (12.288 MHz = 256 fs). Register sequences from the Switch's Linux
//! (CTCaer/switch-l4t-platform-t210-nx tegra210-odin DT, tegra-alt and
//! mainline sound/soc/tegra, sound/soc/codecs/rt5640.c). Clocks and pad
//! power are `clk`'s (`clk::AUDIO`).
//!
//! A client shares a ring of frames; a pump thread copies them, a DMA
//! period at a time, into the ring an ADMA channel loops over to the
//! ADMAIF FIFO (silence when the client has nothing).

#![no_std]
#![no_main]

extern crate alloc;

use alloc::sync::Arc;
use core::sync::atomic::{AtomicU32, Ordering};

use libnx::abi::proto::{audio as proto, clk};
use libnx::abi::{prot, shm};
use libnx::drv::{self, Mmio};
use libnx::i2c::I2c;
use libnx::ipc::{self, Msg};
use libnx::mem::SharedMemory;
use libnx::sync::Mutex;
use libnx::{println, thread, time};

const PINMUX: u64 = 0x7000_3000;
const GPIO: u64 = 0x6000_D000;
const AHUB: u64 = 0x702D_0000;
const ADMA: u64 = 0x702E_2000;

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

/// GPIO port B (1, pins 0..3 = DAP1), port Z (25, pin 4 = codec LDO1_EN),
/// and port BB (27, pin 0 = AUD_MCLK).
const PORT_B: usize = 1;
const PORT_Z: usize = 25;
const PORT_BB: usize = 27;
const GPIO_CNF: usize = 0x00;
const GPIO_OE: usize = 0x10;
const GPIO_OUT: usize = 0x20;
const fn port(reg: usize, p: usize) -> usize {
    reg + (p >> 2) * 0x100 + (p & 3) * 4
}
const DAP1_PINS: u32 = 0xF;
const MCLK_PIN: u32 = 1 << 0;
const LDO1_EN: u32 = 1 << 4;
const PM_DRIVE_4X: u32 = 3 << 13;

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
const ADMAIF_TX_CIF_CTRL: usize = 0x20;
const ADMAIF_TX_FIFO_WRITE: usize = 0x2C;
const ADMAIF_GLOBAL_ENABLE: usize = 0x00;
const ADMAIF_GLOBAL_CG: usize = 0x08;
/// Two 16-bit samples per FIFO word.
const ADMAIF_PACK16: u32 = 1 << 30;

const I2S1: usize = 0x1000;
const I2S_RX_ENABLE: usize = 0x00;
const I2S_RX_SOFT_RESET: usize = 0x04;
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
const RT_DAC1_DIG_VOL: u8 = 0x19;
/// Speaker volume: both channels muted (`0xC8C8`); unmuted attenuation in
/// bits 13:8 (left) and 5:0 (right), 1.5 dB per step.
const SPK_MUTED: u16 = 0xC8C8;
/// DAC1 digital volume: 0xA0 (-5.6 dB) on both channels for clean headroom.
const DAC1_VOL: u16 = 0xA0A0;
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
const RT_PRIV_INDEX: u8 = 0x6A;
const RT_PRIV_DATA: u8 = 0x6C;
const RT_GCTL1: u8 = 0xFA;
const RT_MICBIAS: u8 = 0x93;
const RT_DSP_PATH2: u8 = 0x2E;
const RT_VENDOR_ID: u8 = 0xFD;
const RT_VENDOR_ID2: u8 = 0xFE;

/// Map `0..=100`% volume to `RT_SPK_VOL`: muted at 0, steps 28 (-42 dB)
/// down to 6 (-9 dB) for 1..=100%.
fn spk_vol_reg(vol: u8) -> u16 {
    if vol == 0 {
        return SPK_MUTED;
    }
    let att = (28 - (vol.min(100) as u16 * 22 / 100)).clamp(6, 31);
    att << 8 | att
}

/// Software amplitude scale (0..=256) for fine steps at low volume.
fn sw_gain(vol: u8) -> i32 {
    if vol == 0 {
        0
    } else {
        64 + (vol.min(100) as i32 * 192 / 100)
    }
}

#[derive(Clone, Copy)]
struct Audio {
    i2c: I2c,
    pinmux: Mmio,
    gpio: Mmio,
    ahub: Mmio,
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
        let g = &self.gpio;
        let cnf_b = g.r32(port(GPIO_CNF, PORT_B));
        let cnf_bb = g.r32(port(GPIO_CNF, PORT_BB));
        // Release DAP1 (PB0..PB3) and AUD_MCLK (PBB0) from GPIO override into SFIO mode.
        g.modify(port(GPIO_CNF, PORT_B), DAP1_PINS, 0);
        g.modify(port(GPIO_CNF, PORT_BB), MCLK_PIN, 0);
        println!(
            "[audio] pads: GPIO_CNF B {:#x}->{:#x}, BB {:#x}->{:#x}",
            cnf_b,
            g.r32(port(GPIO_CNF, PORT_B)),
            cnf_bb,
            g.r32(port(GPIO_CNF, PORT_BB))
        );
        // MCLK (function AUD), I2S1's clocks and data out: outputs; data
        // in an input (as hekate `pinmux_config_i2s` and the DT).
        pm.w32(PM_AUD_MCLK, PM_DRIVE_4X | PM_PULL_DOWN);
        for p in [PM_DAP1_FS, PM_DAP1_DOUT, PM_DAP1_SCLK] {
            pm.w32(p, PM_DRIVE_4X | PM_PULL_DOWN);
        }
        pm.w32(
            PM_DAP1_DIN,
            PM_DRIVE_4X | PM_PULL_DOWN | PM_TRISTATE | PM_INPUT,
        );
        // Codec LDO1 enable: pz4 as a GPIO output, high.
        pm.w32(PM_PZ4, PM_PULL_DOWN | 1);
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
        let mut reset_res = Err(-libnx::abi::err::EIO);
        for _ in 0..5 {
            reset_res = self.codec_write(RT_RESET, 0);
            if reset_res.is_ok() {
                break;
            }
            time::sleep_ms(10);
        }
        reset_res?;
        time::sleep_ms(20);
        let (v1, v2) = (
            self.codec_read(RT_VENDOR_ID)?,
            self.codec_read(RT_VENDOR_ID2)?,
        );
        println!("[audio] codec ID {:#06x} {:#06x}", v1, v2);
        // What rt5640's probe and bias level STANDBY write: the private
        // (index / data) registers of its init list, the global control
        // and mic bias bits, DSP path 2.
        for (idx, val) in [
            (0x3d, 0x3600),
            (0x12, 0x0aa8),
            (0x14, 0x0aaa),
            (0x21, 0xe0e0),
            (0x23, 0x1804),
        ] {
            self.codec_write(RT_PRIV_INDEX, idx)?;
            self.codec_write(RT_PRIV_DATA, val)?;
        }
        self.codec_update(RT_GCTL1, 0x0301, 0x0301)?;
        self.codec_update(RT_MICBIAS, 0x0030, 0x0030)?;
        self.codec_update(RT_DSP_PATH2, 0xFC00, 0x0C00)?;
        // References first, then the fast ones (rt5640_vref_event).
        const VREF1: u16 = 1 << 15;
        const VREF2: u16 = 1 << 4;
        const FV1: u16 = 1 << 14;
        const FV2: u16 = 1 << 3;
        const BG: u16 = 1 << 11;
        const LDO2: u16 = 1 << 2;
        self.codec_write(RT_PWR_ANLG1, VREF1 | VREF2 | BG | LDO2 | 1 << 13)?;
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
        self.codec_write(RT_DAC1_DIG_VOL, DAC1_VOL)?;
        // Path: INF1 -> DAC MIX -> Stereo DAC MIX -> SPK MIX -> SPKVOL ->
        // SPO MIX. Only these switches are opened.
        self.codec_update(RT_AD_DA_MIXER, 1 << 14 | 1 << 6, 0)?;
        self.codec_update(RT_STO_DAC_MIXER, 1 << 14 | 1 << 6, 0)?;
        self.codec_update(RT_SPK_L_MIXER, 1 << 3, 0)?;
        self.codec_update(RT_SPK_R_MIXER, 1 << 3, 0)?;
        self.codec_update(RT_SPO_L_MIXER, 1 << 12, 0)?;
        self.codec_update(RT_SPO_R_MIXER, 1 << 12, 0)?;
        // The speakers stay muted until a stream runs: an amplifier left
        // on without a bit clock hisses and buzzes.
        self.codec_write(RT_SPK_VOL, SPK_MUTED)?;
        Ok(())
    }

    /// Hub, codec and DMA state, for the log.
    fn dump(&self, adma: &Mmio, what: &str) {
        let a = &self.ahub;
        let c = |r| self.codec_read(r).unwrap_or(0xFFFF);
        println!(
            "[audio] {}: I2S1 en {:#x} rx {:#x} st {:#x} ctrl {:#x} cif {:#x}; ADMAIF1 en {:#x} st {:#x} cif {:#x}; xbar {:#x}; ADMA st {:#x} xfer {:#x} tc {:#x} src {:#x}",
            what,
            a.r32(I2S1 + I2S_ENABLE),
            a.r32(I2S1 + I2S_RX_ENABLE),
            a.r32(I2S1 + 0x0C),
            a.r32(I2S1 + I2S_CTRL),
            a.r32(I2S1 + I2S_RX_CIF_CTRL),
            a.r32(ADMAIF_TX1 + ADMAIF_TX_ENABLE),
            a.r32(ADMAIF_TX1 + 0x0C),
            a.r32(ADMAIF_TX1 + ADMAIF_TX_CIF_CTRL),
            a.r32(XBAR + XBAR_I2S1_RX1),
            adma.r32(ADMA_CH_STATUS),
            adma.r32(ADMA_CH_XFER_STATUS),
            adma.r32(ADMA_CH_TC),
            adma.r32(ADMA_CH_SRC),
        );
        println!(
            "[audio] codec: dig1 {:#x} anlg1 {:#x} mixer {:#x} vol {:#x} i2s1 {:#x} glb {:#x} adda {:#x} spkvol {:#x} stodac {:#x} spkl {:#x} spol {:#x} adda_mix {:#x}",
            c(RT_PWR_DIG1),
            c(RT_PWR_ANLG1),
            c(RT_PWR_MIXER),
            c(RT_PWR_VOL),
            c(RT_I2S1_SDP),
            c(RT_GLB_CLK),
            c(RT_ADDA_CLK1),
            c(RT_SPK_VOL),
            c(RT_STO_DAC_MIXER),
            c(RT_SPK_L_MIXER),
            c(RT_SPO_L_MIXER),
            c(RT_AD_DA_MIXER),
        );
    }

    /// Hub, pads and codec, once.
    fn up(&self) -> Result<(), i64> {
        libnx::clk::enable(clk::AUDIO).inspect_err(|e| println!("[audio] clk AUDIO: {}", e))?;
        self.pads();
        time::sleep_ms(25);
        self.codec()
            .inspect_err(|e| println!("[audio] codec: {}", e))?;
        self.hub();
        Ok(())
    }
}

/// ADMA (tegra210-adma.c): global registers at 0xc00, channel n at
/// n * 0x80.
const ADMA_GLOBAL: usize = 0xC00;
const ADMA_GLOBAL_CMD: usize = 0x00;
const ADMA_GLOBAL_SOFT_RESET: usize = 0x04;
const ADMA_GLOBAL_INT_CLEAR: usize = 0x20;
const ADMA_CH_CMD: usize = 0x00;
const ADMA_CH_STATUS: usize = 0x0C;
const ADMA_CH_INT_CLEAR: usize = 0x1C;
const ADMA_CH_CTRL: usize = 0x24;
const ADMA_CH_CONFIG: usize = 0x28;
const ADMA_CH_FIFO_CTRL: usize = 0x2C;
const ADMA_CH_SRC: usize = 0x34;
const ADMA_CH_TRG: usize = 0x3C;
const ADMA_CH_TC: usize = 0x44;
const ADMA_CH_XFER_STATUS: usize = 0x54;
/// Memory to AHUB, continuous, flow control; request: ADMAIF channel 1.
const ADMA_CTRL: u32 = 1 << 28 | 4 << 12 | 2 << 8 | 1 << 1;
/// DMA ring: `PERIODS` periods of `PERIOD_FRAMES` frames, a period
/// being one ADMA buffer (the position counts them).
const PERIODS: u32 = 8;
const PERIOD_FRAMES: u32 = 512;
const PERIOD_BYTES: u32 = PERIOD_FRAMES * 4;
/// Burst of 16 words (fls(16) = 5), weight 1; ring of PERIODS buffers.
const ADMA_CONFIG: u32 = (PERIODS - 1) << 28 | 5 << 20 | 1;
/// ADMA FIFO size 3, the transmit field (ADMAIF channel 1).
const ADMA_FIFO_CTRL: u32 = 3 << 8;
/// Silent for this long, the DMA stops until something plays.
const IDLE_NS: u64 = 3_000_000_000;

/// Parabolic sine approximation: `phase` in 0..65536 -> `-32768..=32768`.
fn sin16(phase: u32) -> i32 {
    let p = phase & 0xFFFF;
    let half = p & 0x7FFF;
    let v = ((half * (32768 - half)) >> 13) as i32;
    if p & 0x8000 == 0 {
        v
    } else {
        -v
    }
}

/// Scale packed stereo 16-bit frame `w` by `gain` (0..=256).
fn scale_frame(w: u32, gain: i32) -> u32 {
    let l = ((w as u16 as i16 as i32) * gain >> 8).clamp(-32768, 32767) as i16 as u16 as u32;
    let r =
        (((w >> 16) as u16 as i16 as i32) * gain >> 8).clamp(-32768, 32767) as i16 as u16 as u32;
    l | r << 16
}

/// A client's ring (`proto::RING_*`).
struct Client {
    mem: SharedMemory,
    frames: u32,
}

impl Client {
    fn word(&self, at: usize) -> &AtomicU32 {
        unsafe { &*(self.mem.as_ptr().add(at) as *const AtomicU32) }
    }

    /// Up to `n` frames into `out` (two words per frame, packed as the
    /// FIFO takes them); the count taken.
    fn take(&self, out: &mut [u32], n: u32) -> u32 {
        let w = self.word(proto::RING_WRITE).load(Ordering::Acquire);
        let r = self.word(proto::RING_READ).load(Ordering::Relaxed);
        let avail = w.wrapping_sub(r).min(self.frames).min(n);
        let data = unsafe { self.mem.as_ptr().add(proto::RING_HEADER) as *const u32 };
        for i in 0..avail {
            let at = (r.wrapping_add(i) % self.frames) as usize;
            out[i as usize] = unsafe { core::ptr::read_volatile(data.add(at)) };
        }
        self.word(proto::RING_READ)
            .store(r.wrapping_add(avail), Ordering::Release);
        avail
    }

    fn pending(&self) -> bool {
        self.word(proto::RING_WRITE).load(Ordering::Acquire)
            != self.word(proto::RING_READ).load(Ordering::Relaxed)
    }

    fn underrun(&self) {
        self.word(proto::RING_UNDERRUNS)
            .fetch_add(1, Ordering::Relaxed);
    }
}

/// A test sine.
struct Tone {
    phase: u32,
    step: u32,
    frames: u64,
    amp: i32,
}

impl Tone {
    fn frame(&mut self) -> u32 {
        let v = sin16(self.phase);
        self.phase = self.phase.wrapping_add(self.step);
        let s = (v * self.amp / 32768) as i16 as u16 as u32;
        s | s << 16
    }
}

/// Built-in 2-voice polyphonic melody: `(melody_hz, bass_hz, eighth_notes)`.
const NOTE_FRAMES: u32 = proto::RATE * 135 / 1000;
const SCORE: &[(u16, u16, u8)] = &[
    // Bar 1: C major arpeggio
    (523, 262, 1),
    (659, 262, 1),
    (784, 330, 1),
    (1047, 392, 2),
    (988, 392, 1),
    (784, 330, 2),
    // Bar 2: A minor -> G major
    (880, 220, 1),
    (1047, 220, 1),
    (1319, 330, 2),
    (1175, 294, 1),
    (988, 247, 1),
    (784, 392, 2),
    // Bar 3: F major -> C major
    (698, 349, 1),
    (880, 349, 1),
    (1047, 440, 2),
    (784, 262, 1),
    (659, 330, 1),
    (523, 262, 2),
    // Bar 4: D minor -> G7 turnaround
    (587, 294, 1),
    (698, 294, 1),
    (880, 349, 1),
    (784, 392, 1),
    (659, 330, 1),
    (784, 392, 1),
    (988, 494, 2),
    // Bar 5: Repeat theme higher
    (1047, 262, 1),
    (988, 262, 1),
    (880, 330, 1),
    (784, 392, 2),
    (659, 330, 1),
    (784, 392, 2),
    // Bar 6: F major -> E minor -> D minor -> C majorfinale
    (880, 349, 1),
    (784, 330, 1),
    (698, 294, 1),
    (659, 262, 1),
    (587, 294, 2),
    (784, 392, 2),
    (1047, 523, 4),
];

struct Music {
    idx: usize,
    pos: u32,
    len: u32,
    p_mel: u32,
    p_bass: u32,
    s_mel: u32,
    s_bass: u32,
}

impl Music {
    fn new() -> Self {
        let mut m = Self {
            idx: 0,
            pos: 0,
            len: 0,
            p_mel: 0,
            p_bass: 0,
            s_mel: 0,
            s_bass: 0,
        };
        m.load_note();
        m
    }

    fn total_ms() -> u64 {
        let eighths: u64 = SCORE.iter().map(|n| n.2 as u64).sum();
        eighths * 135
    }

    fn load_note(&mut self) {
        if let Some(&(mel, bass, dur)) = SCORE.get(self.idx) {
            self.pos = 0;
            self.len = dur as u32 * NOTE_FRAMES;
            self.s_mel = (((mel as u64) << 16) / proto::RATE as u64) as u32;
            self.s_bass = (((bass as u64) << 16) / proto::RATE as u64) as u32;
        }
    }

    fn frame(&mut self) -> Option<u32> {
        if self.idx >= SCORE.len() {
            return None;
        }
        // Smooth 8 ms attack and 8 ms release envelope to eliminate note clicks.
        let ramp = 384u32;
        let rem = self.len.saturating_sub(self.pos);
        let env = if self.pos < ramp {
            self.pos * 256 / ramp
        } else if rem < ramp {
            rem * 256 / ramp
        } else {
            let decay = rem * 180 / self.len.max(1);
            76 + decay
        } as i32;

        // Warm chime: fundamental sine + 25% 2nd harmonic.
        let mel = sin16(self.p_mel) + (sin16(self.p_mel << 1) >> 2);
        let bass = sin16(self.p_bass) + (sin16(self.p_bass << 1) >> 3);
        self.p_mel = self.p_mel.wrapping_add(self.s_mel);
        self.p_bass = self.p_bass.wrapping_add(self.s_bass);

        let m = (mel * env >> 8) * 11 / 32;
        let b = (bass * env >> 8) * 9 / 32;
        let l = (m * 3 / 4 + b).clamp(-32768, 32767) as i16 as u16 as u32;
        let r = (m + b * 3 / 4).clamp(-32768, 32767) as i16 as u16 as u32;

        self.pos += 1;
        if self.pos >= self.len {
            self.idx += 1;
            self.load_note();
        }
        Some(l | r << 16)
    }
}

struct Shared {
    client: Option<Client>,
    tone: Option<Tone>,
    music: Option<Music>,
    volume: u8,
    ready: bool,
    playing: bool,
    underruns: u32,
    periods: u32,
    /// Most periods the DMA went on between two looks, and how often it
    /// went a whole lap (the ring played again: a late pump).
    max_step: u32,
    laps: u32,
}

impl Default for Shared {
    fn default() -> Self {
        Self {
            client: None,
            tone: None,
            music: None,
            volume: proto::DEFAULT_VOL,
            ready: false,
            playing: false,
            underruns: 0,
            periods: 0,
            max_step: 0,
            laps: 0,
        }
    }
}

/// The pump: owns the DMA ring and the ADMA channel.
struct Pump {
    hw: Audio,
    ahub: Mmio,
    adma: Mmio,
    ring: SharedMemory,
    /// Periods filled and played since the DMA started; the ADMA's count
    /// (16 bits) at the start and at the last look.
    filled: u32,
    played: u32,
    last: u16,
    quiet_since: u64,
}

impl Pump {
    /// Fill the DMA ring's slot of period `n` with the next frames.
    fn fill(&mut self, sh: &mut Shared, n: u32) -> bool {
        let mut buf = [0u32; PERIOD_FRAMES as usize];
        let mut got = 0u32;
        if let Some(m) = sh.music.as_mut() {
            while got < PERIOD_FRAMES {
                let Some(w) = m.frame() else {
                    sh.music = None;
                    break;
                };
                buf[got as usize] = w;
                got += 1;
            }
        } else if let Some(t) = sh.tone.as_mut() {
            while got < PERIOD_FRAMES && t.frames > 0 {
                buf[got as usize] = t.frame();
                got += 1;
                t.frames -= 1;
            }
            if t.frames == 0 {
                sh.tone = None;
            }
        } else if let Some(c) = sh.client.as_ref() {
            got = c.take(&mut buf, PERIOD_FRAMES);
            if got < PERIOD_FRAMES {
                c.underrun();
                sh.underruns = sh.underruns.wrapping_add(1);
            }
        }
        let gain = sw_gain(sh.volume);
        let at = (n % PERIODS * PERIOD_BYTES) as usize;
        let dst = unsafe { self.ring.as_ptr().add(at) as *mut u32 };
        for (i, w) in buf.iter().enumerate() {
            let scaled = if i < got as usize {
                scale_frame(*w, gain)
            } else {
                0
            };
            unsafe { core::ptr::write_volatile(dst.add(i), scaled) };
        }
        got > 0
    }

    fn begin(&mut self, sh: &mut Shared) {
        for n in 0..PERIODS {
            self.fill(sh, n);
        }
        self.filled = PERIODS;
        self.played = 0;
        let (a, d) = (&self.ahub, &self.adma);
        // The block's reset needs it enabled; the stream path is reset
        // before use, as Linux does.
        a.w32(I2S1 + I2S_ENABLE, 1);
        a.w32(I2S1 + I2S_RX_SOFT_RESET, 1);
        if !time::poll(10_000, || a.r32(I2S1 + I2S_RX_SOFT_RESET) & 1 == 0) {
            println!("[audio] I2S1 RX reset did not finish");
        }
        a.w32(
            I2S1 + I2S_CTRL,
            I2S_CTRL_MASTER | I2S_BITS_16 | I2S_FSYNC_WIDTH_31,
        );
        a.w32(I2S1 + I2S_RX_CTRL, I2S_DATA_OFFSET);
        a.w32(I2S1 + I2S_TIMING, 15);
        a.w32(I2S1 + I2S_RX_CIF_CTRL, cif(3));
        a.w32(I2S1 + I2S_RX_ENABLE, 1);
        a.w32(ADMAIF_TX1 + ADMAIF_TX_CIF_CTRL, ADMAIF_PACK16 | cif(0));
        a.w32(ADMAIF_TX1 + ADMAIF_TX_ENABLE, 1);
        let pa = self.ring.paddr(0).unwrap_or(0);
        d.w32(ADMA_CH_INT_CLEAR, 1);
        d.w32(ADMA_CH_TC, PERIOD_BYTES);
        d.w32(ADMA_CH_CTRL, ADMA_CTRL);
        d.w32(ADMA_CH_SRC, pa as u32);
        d.w32(
            ADMA_CH_TRG,
            (AHUB + (ADMAIF_TX1 + ADMAIF_TX_FIFO_WRITE) as u64) as u32,
        );
        d.w32(ADMA_CH_FIFO_CTRL, ADMA_FIFO_CTRL);
        d.w32(ADMA_CH_CONFIG, ADMA_CONFIG);
        d.w32(ADMA_CH_CMD, 1);
        // The count may start again from zero with the channel: take the
        // reference after it runs (a period lasts 2.7 ms).
        time::sleep_us(100);
        self.last = d.r32(ADMA_CH_XFER_STATUS) as u16;
        self.quiet_since = time::now_ns();
        // The stream is running: let it through (the pops of the
        // amplifier go by while the first periods play).
        time::sleep_ms(2);
        let _ = self.hw.codec_write(RT_SPK_VOL, spk_vol_reg(sh.volume));
        sh.playing = true;
    }

    fn end(&mut self, sh: &mut Shared) {
        let _ = self.hw.codec_write(RT_SPK_VOL, SPK_MUTED);
        let d = &self.adma;
        d.w32(ADMA_CH_CMD, 0);
        time::poll(10_000, || d.r32(ADMA_CH_STATUS) & 1 == 0);
        // ADMAIF stop as Linux's tegra_admaif_stop: disable, wait, reset.
        let a = &self.ahub;
        a.w32(ADMAIF_TX1 + ADMAIF_TX_ENABLE, 0);
        time::poll(10_000, || a.r32(ADMAIF_TX1 + 0x0C) & 1 == 0);
        a.w32(ADMAIF_TX1 + 0x04, 1);
        time::poll(10_000, || a.r32(ADMAIF_TX1 + 0x04) & 1 == 0);
        a.w32(I2S1 + I2S_RX_ENABLE, 0);
        sh.playing = false;
        println!("[audio] idle: stopped after {} periods", self.played);
    }

    /// One look, every millisecond or so.
    fn tick(&mut self, sh: &mut Shared) {
        let active = sh.music.is_some()
            || sh.tone.is_some()
            || sh.client.as_ref().is_some_and(|c| c.pending());
        if !sh.playing {
            if active {
                self.begin(sh);
            }
            return;
        }
        let now = self.adma.r32(ADMA_CH_XFER_STATUS) as u16;
        let step = now.wrapping_sub(self.last) as u32;
        sh.max_step = sh.max_step.max(step);
        if step >= PERIODS {
            sh.laps += 1;
        }
        self.played = self.played.wrapping_add(step);
        self.last = now;
        sh.periods = self.played;
        let mut loud = false;
        // A period played is a slot to refill with the frames to come.
        while self.filled.wrapping_sub(self.played) < PERIODS {
            loud |= self.fill(sh, self.filled);
            self.filled = self.filled.wrapping_add(1);
        }
        let t = time::now_ns();
        if loud || active {
            self.quiet_since = t;
        } else if t - self.quiet_since > IDLE_NS {
            self.end(sh);
        }
    }
}

libnx::entry!(main);

fn main() -> i32 {
    let (Ok(pinmux), Ok(gpio), Ok(ahub), Ok(adma)) = (
        Mmio::map(drv::dev_slot(0), PINMUX, 0x1000),
        Mmio::map(drv::dev_slot(1), GPIO, 0x1000),
        Mmio::map(drv::dev_slot(2), AHUB, 0x2000),
        Mmio::map(drv::dev_slot(3), ADMA, 0x1000),
    ) else {
        println!("[audio] no register access");
        return 1;
    };
    let Ok(i2c) = I2c::connect() else {
        println!("[audio] no i2c service");
        return 1;
    };
    let hw = Audio {
        i2c,
        pinmux,
        gpio,
        ahub,
    };
    let shared = Arc::new(Mutex::new(Shared::default()));
    // The hardware is touched by the first request, not at boot.
    let ep = drv::publish("audio").expect("audio: publish");
    println!("[audio] service ready (off)");
    drv::serve(ep, |rx| {
        let m = &rx.msg;
        let handle =
            (m.label() == proto::OPEN && m.handles() == 1).then(|| ipc::received_handle(0));
        let mut sh = shared.lock();
        if m.label() == proto::VOLUME && m.mr[0] == proto::VOL_GET {
            return Some(Msg::from_words(0, &[sh.volume as u64]));
        }
        if !sh.ready {
            let r = hw.up().and_then(|_| {
                // The DMA ring (a few frames, below 4 GiB, uncached) and
                // the ADMA, reset and enabled.
                let ring = SharedMemory::new(
                    (PERIODS * PERIOD_BYTES) as usize,
                    shm::CONTIGUOUS | shm::UNCACHED | shm::DMA32,
                )?;
                let d = &adma;
                d.w32(ADMA_GLOBAL + ADMA_GLOBAL_INT_CLEAR, 1);
                d.w32(ADMA_GLOBAL + ADMA_GLOBAL_SOFT_RESET, 1);
                time::poll(10_000, || d.r32(ADMA_GLOBAL + ADMA_GLOBAL_SOFT_RESET) == 0);
                d.w32(ADMA_GLOBAL + ADMA_GLOBAL_CMD, 1);
                let mut pump = Pump {
                    hw,
                    ahub,
                    adma,
                    ring,
                    filled: 0,
                    played: 0,
                    last: 0,
                    quiet_since: 0,
                };
                let sh2 = shared.clone();
                thread::spawn_on(None, 6, move || loop {
                    time::sleep_us(1000);
                    let mut s = sh2.lock();
                    pump.tick(&mut s);
                })
                .map(|_| ())
            });
            match r {
                Ok(()) => sh.ready = true,
                Err(e) => {
                    println!("[audio] power-up failed: error {}", e);
                    if let Some(h) = handle {
                        let _ = libnx::sys::handle_close(h);
                    }
                    return Some(Msg::error(e));
                }
            }
        }
        match m.label() {
            proto::OPEN => {
                let Some(h) = handle else {
                    return Some(Msg::error(-libnx::abi::err::EINVAL));
                };
                let len = m.mr[0] as usize;
                let frames = len.saturating_sub(proto::RING_HEADER) / 4;
                match SharedMemory::map(h, len, prot::READ | prot::WRITE) {
                    Ok(mem) if frames >= PERIOD_FRAMES as usize => {
                        sh.client = Some(Client {
                            mem,
                            frames: frames as u32,
                        });
                        Some(Msg::from_words(0, &[frames as u64]))
                    }
                    Ok(_) => Some(Msg::error(-libnx::abi::err::EINVAL)),
                    Err(e) => {
                        let _ = libnx::sys::handle_close(h);
                        Some(Msg::error(e))
                    }
                }
            }
            proto::CLOSE => {
                sh.client = None;
                Some(Msg::new(0, 0, 0))
            }
            proto::TONE => {
                let (hz, ms, level) = (m.mr[0] as u32, m.mr[1], m.mr[2] as u32);
                sh.music = None;
                sh.tone = Some(Tone {
                    phase: 0,
                    step: (((hz.min(20_000) as u64) << 16) / proto::RATE as u64) as u32,
                    frames: ms * proto::RATE as u64 / 1000,
                    amp: (level.min(100) * 300) as i32,
                });
                Some(Msg::from_words(0, &[ms]))
            }
            proto::MUSIC => {
                sh.tone = None;
                sh.music = Some(Music::new());
                Some(Msg::from_words(0, &[Music::total_ms()]))
            }
            proto::VOLUME => {
                if m.mr[0] != proto::VOL_GET {
                    sh.volume = (m.mr[0] as u8).min(100);
                    if sh.playing {
                        let _ = hw.codec_write(RT_SPK_VOL, spk_vol_reg(sh.volume));
                    }
                }
                Some(Msg::from_words(0, &[sh.volume as u64]))
            }
            proto::STATUS => {
                println!(
                    "[audio] client {}, tone {}, music {}, vol {}%, playing {}, underruns {}, periods {}, max step {}, laps {}",
                    sh.client.is_some(),
                    sh.tone.is_some(),
                    sh.music.is_some(),
                    sh.volume,
                    sh.playing,
                    sh.underruns,
                    sh.periods,
                    sh.max_step,
                    sh.laps
                );
                hw.dump(&adma, "status");
                Some(Msg::from_words(
                    0,
                    &[
                        proto::ST_READY | if sh.playing { proto::ST_PLAYING } else { 0 },
                        sh.underruns as u64,
                        sh.periods as u64,
                        sh.volume as u64,
                    ],
                ))
            }
            _ => Some(Msg::error(-libnx::abi::err::EINVAL)),
        }
    })
}
