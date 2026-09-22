//! sd: SDMMC1 card (SDHCI); 512-byte ADMA2 blocks via `blk` protocol.
//! Powers the slot itself (`board`), then Tegra pad trims and calibration as hekate.

#![no_std]
#![no_main]

extern crate alloc;

mod board;

use libnx::abi::err::{EINVAL, EIO, ETIMEDOUT};
use libnx::abi::prot;
use libnx::abi::proto::{blk, clk};
use libnx::abi::shm::{CONTIGUOUS, DMA32, UNCACHED};
use libnx::drv::{self, Irq, Mmio};
use libnx::ipc::{self, Msg};
use libnx::mem::SharedMemory;
use libnx::{ns, println, sys, time};

use board::Board;

const SDMMC1: u64 = 0x700B_0000;
/// Controller clock set by `clk` (PLLP / 8.5); CAPABILITIES' base clock
/// field is not reliable on Tegra.
const BASE_KHZ: u32 = 48_000;

// SDHCI registers.
const BLOCK_SIZE: usize = 0x04;
const BLOCK_COUNT: usize = 0x06;
const ARGUMENT: usize = 0x08;
const TRANSFER_MODE: usize = 0x0C;
const COMMAND: usize = 0x0E;
const RESPONSE: usize = 0x10;
const PRESENT_STATE: usize = 0x24;
const HOST_CONTROL: usize = 0x28;
const POWER_CONTROL: usize = 0x29;
const CLOCK_CONTROL: usize = 0x2C;
const TIMEOUT_CONTROL: usize = 0x2E;
const SOFTWARE_RESET: usize = 0x2F;
const INT_STATUS: usize = 0x30;
const INT_ENABLE: usize = 0x34;
const INT_SIGNAL: usize = 0x38;
const HOST_CONTROL2: usize = 0x3E;
const ADMA_ADDRESS: usize = 0x58;

// Tegra vendor registers (hekate bdk/storage/sdmmc_t210.h).
const VENDOR_CLOCK_CNTRL: usize = 0x100;
const VENDOR_TUNING_CNTRL0: usize = 0x1C0;
const VENDOR_IO_TRIM_CNTRL: usize = 0x1AC;
const SDMEMCOMPPADCTRL: usize = 0x1E0;
const AUTO_CAL_CONFIG: usize = 0x1E4;
const AUTO_CAL_STATUS: usize = 0x1EC;
const IO_SPARE: usize = 0x1F0;
const PADCTRL_E_INPUT_PWRD: u32 = 1 << 31;
const AUTOCAL_ENABLE: u32 = 1 << 29;
const AUTOCAL_START: u32 = 1 << 31;
const AUTOCAL_ACTIVE: u32 = 1 << 31;
const TUNING_TAP_HW_UPDATED: u32 = 1 << 17;
const PRESET_VAL_EN: u16 = 1 << 15;
const CLOCK_CARD_EN: u16 = 1 << 2;

const INT_CMD_DONE: u32 = 1 << 0;
const INT_XFER_DONE: u32 = 1 << 1;
const INT_ERROR: u32 = 1 << 15;

const BLOCK: usize = 512;
/// DMA buffer: one descriptor page then 256 KiB of data.
const DESC_BYTES: usize = 4096;
const MAX_BLOCKS: usize = 512;

#[derive(Clone, Copy, PartialEq)]
enum Rsp {
    None,
    R1,
    R1b,
    R2,
    R3,
}

struct Card {
    regs: Mmio,
    irq: Irq,
    dma: SharedMemory,
    dma_pa: u64,
    rca: u32,
    /// Block addressing (SDHC/SDXC) vs byte addressing (SDSC).
    block_addressed: bool,
    blocks: u64,
}

impl Card {
    fn reset(&self, what: u8) {
        self.regs.w8(SOFTWARE_RESET, what);
        let end = time::now_ns() + 100_000_000;
        while self.regs.r8(SOFTWARE_RESET) & what != 0 && time::now_ns() < end {}
    }

    fn set_clock(&self, khz: u32) -> Result<(), i64> {
        self.regs.w16(CLOCK_CONTROL, 0);
        let div = if khz >= BASE_KHZ {
            0
        } else {
            BASE_KHZ.div_ceil(2 * khz)
        };
        let v = ((div & 0xFF) << 8 | ((div >> 8) & 3) << 6 | 1) as u16;
        self.regs.w16(CLOCK_CONTROL, v);
        let end = time::now_ns() + 20_000_000;
        while self.regs.r16(CLOCK_CONTROL) & 2 == 0 {
            if time::now_ns() > end {
                return Err(-ETIMEDOUT); // internal clock never stable
            }
        }
        self.regs.w16(CLOCK_CONTROL, v | CLOCK_CARD_EN);
        Ok(())
    }

    /// Pad trims (T210: trim 2, tap 4, vref 7; T210B01: 14, 11, 0) and
    /// 3.3 V drive auto-calibration, hekate values; SD clock off meanwhile.
    fn tegra_pads(&self, board: &Board) {
        let (trim, tap, vref) = if board.mariko { (14, 11, 0) } else { (2, 4, 7) };
        self.regs.modify(IO_SPARE, 0, 1 << 19); // 1-cycle delayed cmd_oen
        self.regs.modify(VENDOR_IO_TRIM_CNTRL, 1 << 2, 0); // band gap supplies the DLL
        self.regs
            .modify(VENDOR_CLOCK_CNTRL, 0x1FFF_0004, trim << 24 | tap << 16 | 1);
        self.regs
            .modify(VENDOR_TUNING_CNTRL0, TUNING_TAP_HW_UPDATED, 0);
        self.regs.modify(SDMEMCOMPPADCTRL, 0xF, vref);
        // Offsets: T210 pull-down -3 (7-bit two's complement), pull-up 0.
        let (pd, pu) = if board.mariko { (0, 0) } else { (0x7D, 0) };
        self.regs.modify(AUTO_CAL_CONFIG, 0x7F7F, pd << 8 | pu);

        self.regs.modify(SDMEMCOMPPADCTRL, 0, PADCTRL_E_INPUT_PWRD);
        time::sleep_us(1);
        self.regs
            .modify(AUTO_CAL_CONFIG, 0, AUTOCAL_ENABLE | AUTOCAL_START);
        time::sleep_us(2);
        if !self.regs.wait(AUTO_CAL_STATUS, AUTOCAL_ACTIVE, 0, 10_000) {
            self.regs.modify(AUTO_CAL_CONFIG, AUTOCAL_ENABLE, 0);
            board.pad_fallback();
            println!("[sd] pad calibration timed out: standard drive strengths");
        }
        self.regs.modify(SDMEMCOMPPADCTRL, PADCTRL_E_INPUT_PWRD, 0);
    }

    /// Wait for one of the `mask` status bits; the IRQ wakes the thread
    /// for transfers, else short busy wait.
    fn wait_int(&self, mask: u32, timeout_ms: u64) -> Result<u32, i64> {
        let end = time::now_ns() + timeout_ms * 1_000_000;
        loop {
            let st = self.regs.r32(INT_STATUS);
            if st & INT_ERROR != 0 {
                self.regs.w32(INT_STATUS, st);
                self.reset(0x6); // CMD and DAT lines
                return Err(-EIO);
            }
            if st & mask != 0 {
                self.regs.w32(INT_STATUS, st & mask);
                return Ok(st);
            }
            let now = time::now_ns();
            if now > end {
                self.reset(0x6);
                return Err(-ETIMEDOUT);
            }
            if mask & INT_XFER_DONE != 0 {
                self.irq.wait((end - now).max(1));
                self.irq.ack();
            } else {
                core::hint::spin_loop();
            }
        }
    }

    fn command(
        &self,
        idx: u8,
        arg: u32,
        rsp: Rsp,
        data: Option<(usize, bool)>,
    ) -> Result<[u32; 4], i64> {
        let busy = rsp == Rsp::R1b || data.is_some();
        let inhibit = if busy { 3 } else { 1 };
        let end = time::now_ns() + 100_000_000;
        while self.regs.r32(PRESENT_STATE) & inhibit != 0 {
            if time::now_ns() > end {
                return Err(-ETIMEDOUT);
            }
        }
        self.regs.w32(INT_STATUS, 0xFFFF_FFFF);
        let mut mode = 0u16;
        if let Some((n, read)) = data {
            self.write_descriptors(n * BLOCK);
            self.regs.w32(ADMA_ADDRESS, self.dma_pa as u32);
            self.regs
                .w8(HOST_CONTROL, (self.regs.r8(HOST_CONTROL) & !0x18) | 0x10); // 32-bit ADMA2
            self.regs.w16(BLOCK_SIZE, BLOCK as u16 | 7 << 12);
            self.regs.w16(BLOCK_COUNT, n as u16);
            mode = 1 | 2; // DMA, block count
            if n > 1 {
                mode |= 1 << 5 | 1 << 2; // multi-block, auto CMD12
            }
            if read {
                mode |= 1 << 4;
            }
        }
        let flags: u16 = match rsp {
            Rsp::None => 0,
            Rsp::R1 => 0x1A,
            Rsp::R1b => 0x1B,
            Rsp::R2 => 0x09,
            Rsp::R3 => 0x02,
        } | if data.is_some() { 1 << 5 } else { 0 };
        self.regs.w32(ARGUMENT, arg);
        self.regs.w16(TRANSFER_MODE, mode);
        self.regs.w16(COMMAND, (idx as u16) << 8 | flags);
        self.wait_int(INT_CMD_DONE, 1000)?;
        let r = [
            self.regs.r32(RESPONSE),
            self.regs.r32(RESPONSE + 4),
            self.regs.r32(RESPONSE + 8),
            self.regs.r32(RESPONSE + 12),
        ];
        if busy {
            self.wait_int(INT_XFER_DONE, 5000)?;
        }
        Ok(r)
    }

    /// ADMA2 (32-bit) descriptors covering `len` buffer bytes.
    fn write_descriptors(&self, len: usize) {
        let d = self.dma.as_ptr() as *mut u32;
        let mut off = 0;
        let mut k = 0;
        while off < len {
            let n = (len - off).min(65536);
            let last = off + n >= len;
            let attr = 1 | 0x20 | if last { 2 } else { 0 }; // valid, transfer, end
            unsafe {
                d.add(2 * k)
                    .write_volatile(((n as u32 & 0xFFFF) << 16) | attr);
                d.add(2 * k + 1)
                    .write_volatile((self.dma_pa + (DESC_BYTES + off) as u64) as u32);
            }
            off += n;
            k += 1;
        }
    }

    fn app_command(&self, idx: u8, arg: u32, rsp: Rsp) -> Result<[u32; 4], i64> {
        self.command(55, self.rca << 16, Rsp::R1, None)?;
        self.command(idx, arg, rsp, None)
    }

    /// Controller already clocked and out of reset, slot powered.
    fn init(&mut self, board: &Board) -> Result<(), i64> {
        self.reset(0x1);
        self.tegra_pads(board);
        self.regs
            .w16(HOST_CONTROL2, self.regs.r16(HOST_CONTROL2) & !PRESET_VAL_EN);
        self.regs.w8(HOST_CONTROL, 0); // 1-bit bus, default speed
        self.regs.w8(POWER_CONTROL, 0x0F); // 3.3 V, bus power on
        self.set_clock(400)?;
        self.regs.w8(TIMEOUT_CONTROL, 0x0E);
        self.regs.w16(INT_ENABLE, 0xFFFF);
        self.regs.w16(INT_ENABLE + 2, 0xFFFF);
        self.regs
            .w16(INT_SIGNAL, (INT_XFER_DONE | INT_ERROR) as u16);
        self.regs.w16(INT_SIGNAL + 2, 0xFFFF);
        time::sleep_ms(2);

        self.command(0, 0, Rsp::None, None)?;
        let v2 = self
            .command(8, 0x1AA, Rsp::R1, None)
            .map(|r| r[0] & 0xFFF == 0x1AA)
            .unwrap_or(false);
        let end = time::now_ns() + 1_000_000_000;
        let ocr = loop {
            let r = self.app_command(41, if v2 { 0x40FF_8000 } else { 0x00FF_8000 }, Rsp::R3)?;
            if r[0] & (1 << 31) != 0 {
                break r[0];
            }
            if time::now_ns() > end {
                return Err(-ETIMEDOUT);
            }
            time::sleep_ms(5);
        };
        self.block_addressed = ocr & (1 << 30) != 0;
        self.command(2, 0, Rsp::R2, None)?;
        self.rca = self.command(3, 0, Rsp::R1, None)?[0] >> 16;
        let csd = self.command(9, self.rca << 16, Rsp::R2, None)?;
        self.blocks = if (csd[3] >> 22) & 3 == 1 {
            let c_size = ((csd[1] >> 8) & 0x3F_FFFF) as u64;
            (c_size + 1) * 1024
        } else {
            let c_size = (((csd[2] & 3) << 10) | (csd[1] >> 22)) as u64;
            let mult = ((csd[1] >> 7) & 7) as u64;
            let bl_len = ((csd[2] >> 8) & 0xF) as u64;
            ((c_size + 1) << (mult + 2)) << bl_len >> 9
        };
        self.command(7, self.rca << 16, Rsp::R1b, None)?;
        self.app_command(6, 2, Rsp::R1)?; // 4-bit bus
        self.regs.w8(HOST_CONTROL, self.regs.r8(HOST_CONTROL) | 2);
        if !self.block_addressed {
            self.command(16, BLOCK as u32, Rsp::R1, None)?;
        }
        self.set_clock(25_000)
    }

    /// SD clock and bus power off before the slot loses power.
    fn stop(&self) {
        self.regs.w16(CLOCK_CONTROL, 0);
        self.regs.w8(POWER_CONTROL, 0);
    }

    fn transfer(&self, lba: u64, n: usize, read: bool) -> Result<(), i64> {
        if n == 0 || n > MAX_BLOCKS || lba + n as u64 > self.blocks {
            return Err(-EINVAL);
        }
        let arg = if self.block_addressed {
            lba
        } else {
            lba * BLOCK as u64
        } as u32;
        let idx = match (read, n > 1) {
            (true, false) => 17,
            (true, true) => 18,
            (false, false) => 24,
            (false, true) => 25,
        };
        self.command(idx, arg, Rsp::R1, Some((n, read))).map(|_| ())
    }

    fn data(&self) -> &mut [u8] {
        &mut self.dma.as_mut_slice()[DESC_BYTES..]
    }
}

libnx::entry!(main);

fn map(slot: u32, phys: u64) -> Mmio {
    Mmio::map(drv::dev_slot(slot), phys, 0x1000).expect("sd: registers")
}

/// Slot power then controller clock. Card detect only reports: a card
/// that answers beats a detect line read wrong.
fn bring_up(board: &Board) -> Result<(), i64> {
    if !board.card_present() {
        println!("[sd] no card detected, probing anyway");
    }
    board.power_on()?;
    let c = ns::wait_for_timeout("clk", 2_000_000_000)?;
    let mut m = Msg::new(clk::ENABLE, 1, 0);
    m.mr[0] = clk::SDMMC1;
    let r = ipc::call(c, &m).and_then(ipc::checked);
    let _ = sys::handle_close(c);
    r.map(|_| ())
}

fn main() -> i32 {
    let regs = map(0, SDMMC1);
    let board = Board::new(
        map(1, board::APB_MISC),
        map(2, board::PINMUX),
        map(3, board::GPIO),
        map(4, board::PMC_PAGE),
    );
    let irq = Irq::new(drv::dev_slot(5)).expect("sd: interrupt");
    let dma = SharedMemory::new(
        DESC_BYTES + MAX_BLOCKS * BLOCK,
        CONTIGUOUS | UNCACHED | DMA32,
    )
    .expect("sd: DMA buffer");
    let dma_pa = dma.paddr(0).expect("sd: DMA address");
    let mut card = Card {
        regs,
        irq,
        dma,
        dma_pa,
        rca: 0,
        block_addressed: false,
        blocks: 0,
    };
    if let Err(e) = bring_up(&board).and_then(|()| card.init(&board)) {
        println!("[sd] no usable card ({})", libnx::abi::err::name(e));
        card.stop();
        board.power_off();
        loop {
            time::sleep_ms(3_600_000);
        }
    }
    println!(
        "[sd] {} card of {} MiB ({} blocks)",
        if card.block_addressed {
            "SDHC/SDXC"
        } else {
            "SDSC"
        },
        card.blocks / 2048,
        card.blocks
    );
    let ep = drv::publish("blk:sd").expect("sd: publish");
    let mut client: Option<SharedMemory> = None;
    drv::serve(ep, |rx| {
        let m = &rx.msg;
        Some(match m.label() {
            blk::INFO => {
                let mut r = Msg::new(0, 2, 0);
                r.mr[0] = BLOCK as u64;
                r.mr[1] = card.blocks;
                r
            }
            blk::ATTACH if m.handles() == 1 => {
                let h = ipc::received_handle(0);
                client = None;
                match SharedMemory::map(h, MAX_BLOCKS * BLOCK, prot::READ | prot::WRITE) {
                    Ok(s) => {
                        client = Some(s);
                        Msg::new(0, 0, 0)
                    }
                    Err(e) => {
                        let _ = sys::handle_close(h);
                        Msg::error(e)
                    }
                }
            }
            op @ (blk::READ | blk::WRITE) => {
                let (lba, n, off) = (m.mr[0], m.mr[1] as usize, m.mr[2] as usize);
                let Some(buf) = client.as_ref() else {
                    return Some(Msg::error(-EINVAL));
                };
                let len = n * BLOCK;
                if off.checked_add(len).is_none_or(|e| e > buf.len()) || n > MAX_BLOCKS {
                    return Some(Msg::error(-EINVAL));
                }
                let r = if op == blk::READ {
                    card.transfer(lba, n, true).map(|_| {
                        buf.as_mut_slice()[off..off + len].copy_from_slice(&card.data()[..len])
                    })
                } else {
                    card.data()[..len].copy_from_slice(&buf.as_slice()[off..off + len]);
                    card.transfer(lba, n, false)
                };
                match r {
                    Ok(()) => Msg::new(0, 0, 0),
                    Err(e) => Msg::error(e),
                }
            }
            _ => Msg::error(-EINVAL),
        })
    })
}
