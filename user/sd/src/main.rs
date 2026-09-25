//! sd: SDMMC1 card (SDHCI); 512-byte ADMA2 blocks via `blk` protocol,
//! straight into the client's buffer.
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
use libnx::{println, sys, time};

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
const ADMA_ERROR: usize = 0x54;
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
const VDD_180: u16 = 1 << 3;
const CLOCK_CARD_EN: u16 = 1 << 2;

const INT_CMD_DONE: u32 = 1 << 0;
const INT_XFER_DONE: u32 = 1 << 1;
const INT_ERROR: u32 = 1 << 15;

const BLOCK: usize = 512;
const MAX_BLOCKS: usize = 512;
/// Client buffer size (`blk::ATTACH`).
const CLIENT_LEN: usize = MAX_BLOCKS * BLOCK;
/// Own DMA page: ADMA2 descriptors, then block 0 for the first read.
const PAGE: usize = 4096;
const SCRATCH: usize = 2048;

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
    fn tegra_pads(&self, board: &Board, v18: bool) {
        let (trim, tap, vref) = if board.mariko { (14, 11, 0) } else { (2, 4, 7) };
        self.regs.modify(IO_SPARE, 0, 1 << 19); // 1-cycle delayed cmd_oen
        self.regs.modify(VENDOR_IO_TRIM_CNTRL, 1 << 2, 0); // band gap supplies the DLL
        self.regs
            .modify(VENDOR_CLOCK_CNTRL, 0x1FFF_0004, trim << 24 | tap << 16 | 1);
        self.regs
            .modify(VENDOR_TUNING_CNTRL0, TUNING_TAP_HW_UPDATED, 0);
        self.regs.modify(SDMEMCOMPPADCTRL, 0xF, vref);
        // Offsets: T210 pull-down -3 (7-bit two's complement), pull-up 0.
        let (pd, pu) = match (board.mariko, v18) {
            (true, _) => (0, 0),
            (false, false) => (0x7D, 0),
            (false, true) => (0x7B, 0x7B), // -5, -5
        };
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

    /// Wait for one of the `mask` status bits: the IRQ wakes the thread
    /// for transfers, commands are polled (`time::poll`).
    fn wait_int(&self, mask: u32, timeout_ms: u64) -> Result<u32, i64> {
        let hit = || self.regs.r32(INT_STATUS) & (mask | INT_ERROR) != 0;
        if mask & INT_XFER_DONE != 0 {
            let end = time::now_ns() + timeout_ms * 1_000_000;
            while !hit() {
                let now = time::now_ns();
                if now > end {
                    break;
                }
                self.irq.wait((end - now).max(1));
                self.irq.ack();
            }
        } else {
            time::poll(timeout_ms * 1000, hit);
        }
        let st = self.regs.r32(INT_STATUS);
        if st & INT_ERROR != 0 {
            self.report(mask, st, "error");
            self.regs.w32(INT_STATUS, st);
            self.reset(0x6); // CMD and DAT lines
            return Err(-EIO);
        }
        if st & mask != 0 {
            self.regs.w32(INT_STATUS, st & mask);
            return Ok(st);
        }
        self.report(mask, st, "timeout");
        self.reset(0x6);
        Err(-ETIMEDOUT)
    }

    /// A data transfer that failed, as the controller saw it before the
    /// reset: interrupt and line state, the ADMA engine's error and the
    /// address it had reached, against our buffer's. Command-only waits
    /// fail in normal probing and stay quiet.
    fn report(&self, mask: u32, st: u32, what: &str) {
        if mask & INT_XFER_DONE == 0 {
            return;
        }
        println!(
            "[sd] transfer {}: status {:#010x}, present {:#010x}, ADMA error {:#x} at {:#010x}, buffer {:#010x}",
            what,
            st,
            self.regs.r32(PRESENT_STATE),
            self.regs.r32(ADMA_ERROR),
            self.regs.r32(ADMA_ADDRESS),
            unsafe { (self.dma.as_ptr() as *const u32).add(1).read_volatile() }
        );
    }

    fn command(
        &self,
        idx: u8,
        arg: u32,
        rsp: Rsp,
        data: Option<(u64, usize, bool)>,
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
        if let Some((pa, n, read)) = data {
            self.write_descriptors(pa, n * BLOCK);
            // Descriptors and buffer (uncached) in memory before the
            // controller fetches them.
            unsafe { core::arch::asm!("dsb st", options(nostack, preserves_flags)) };
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

    /// ADMA2 (32-bit) descriptors covering `len` bytes at `pa`.
    fn write_descriptors(&self, pa: u64, len: usize) {
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
                d.add(2 * k + 1).write_volatile((pa + off as u64) as u32);
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
    /// Card init at `v18` (1.8 V signaling) or 3.3 V, on a 4-bit bus or
    /// a 1-bit one.
    fn init(&mut self, board: &Board, v18: bool, bus4: bool) -> Result<(), i64> {
        self.reset(0x1);
        self.tegra_pads(board, v18);
        let ctrl2 = self.regs.r16(HOST_CONTROL2) & !(PRESET_VAL_EN | VDD_180);
        self.regs
            .w16(HOST_CONTROL2, ctrl2 | if v18 { VDD_180 } else { 0 });
        self.regs.w8(HOST_CONTROL, 0); // 1-bit bus, default speed
                                       // Bus voltage 3.3 V or 1.8 V, bus power on.
        self.regs.w8(POWER_CONTROL, if v18 { 0x0B } else { 0x0F });
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
        if bus4 {
            self.app_command(6, 2, Rsp::R1)?;
            self.regs.w8(HOST_CONTROL, self.regs.r8(HOST_CONTROL) | 2);
        }
        if !self.block_addressed {
            self.command(16, BLOCK as u32, Rsp::R1, None)?;
        }
        self.set_clock(25_000)
    }

    /// Card and host down to a 1-bit bus, card kept powered and selected:
    /// a slot whose DAT1-3 contacts are loose still reads on DAT0.
    fn bus_1bit(&self) -> Result<(), i64> {
        let state = self.command(13, self.rca << 16, Rsp::R1, None)?[0] >> 9 & 0xF;
        // ACMD6 needs the transfer state: stop a card still sending (5)
        // or receiving (6) the failed block.
        if state == 5 || state == 6 {
            self.command(12, 0, Rsp::R1b, None)?;
        }
        self.app_command(6, 0, Rsp::R1)?;
        self.regs.w8(HOST_CONTROL, self.regs.r8(HOST_CONTROL) & !2);
        Ok(())
    }

    fn bus_width(&self) -> u32 {
        if self.regs.r8(HOST_CONTROL) & 2 != 0 {
            4
        } else {
            1
        }
    }

    /// SD clock and bus power off before the slot loses power.
    fn stop(&self) {
        self.regs.w16(CLOCK_CONTROL, 0);
        self.regs.w8(POWER_CONTROL, 0);
    }

    /// `n` blocks between the card and physical address `pa`.
    fn transfer(&self, lba: u64, n: usize, read: bool, pa: u64) -> Result<(), i64> {
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
        self.command(idx, arg, Rsp::R1, Some((pa, n, read)))
            .map(|_| ())
    }

    /// Block 0 into the own page: does data come at all?
    fn read_first(&self) -> Result<(), i64> {
        self.transfer(0, 1, true, self.dma_pa + SCRATCH as u64)
    }

    /// Host and Tegra pad registers, then the card's state (CMD13), for
    /// a failed transfer.
    fn describe(&self) -> alloc::string::String {
        let r = &self.regs;
        let card = match self.command(13, self.rca << 16, Rsp::R1, None) {
            Ok(r) => alloc::format!("status {:#x} (state {})", r[0], r[0] >> 9 & 0xF),
            Err(e) => alloc::format!("CMD13 {}", libnx::abi::err::name(e)),
        };
        alloc::format!(
            "host ctrl {:#x} power {:#x} clock {:#x} ctrl2 {:#x} vendor clk {:#x} io trim {:#x} spare {:#x} pad {:#x} autocal {:#x}/{:#x}; card {}",
            r.r8(HOST_CONTROL),
            r.r8(POWER_CONTROL),
            r.r16(CLOCK_CONTROL),
            r.r16(HOST_CONTROL2),
            r.r32(VENDOR_CLOCK_CNTRL),
            r.r32(VENDOR_IO_TRIM_CNTRL),
            r.r32(IO_SPARE),
            r.r32(SDMEMCOMPPADCTRL),
            r.r32(AUTO_CAL_CONFIG),
            r.r32(AUTO_CAL_STATUS),
            card
        )
    }
}

/// Client transfer buffer, which the controller reads and writes
/// directly: contiguous, below 4 GiB, uncached (`blk::ATTACH`).
struct Client {
    /// The mapping pins the pages: DMA never lands in freed memory.
    _map: SharedMemory,
    pa: u64,
}

impl Client {
    fn attach(h: u32) -> Result<Self, i64> {
        let map = SharedMemory::map(h, CLIENT_LEN, prot::READ).inspect_err(|_| {
            let _ = sys::handle_close(h);
        })?;
        let pa = map.paddr(0)?;
        for k in 1..CLIENT_LEN / PAGE {
            if map.paddr(k * PAGE)? != pa + (k * PAGE) as u64 {
                return Err(-EINVAL);
            }
        }
        if pa + CLIENT_LEN as u64 > 1 << 32 {
            return Err(-EINVAL);
        }
        Ok(Self { _map: map, pa })
    }
}

libnx::entry!(main);

fn map(slot: u32, phys: u64) -> Mmio {
    Mmio::map(drv::dev_slot(slot), phys, 0x1000).expect("sd: registers")
}

/// Slot power then controller clock. Card detect only reports: a card
/// that answers beats a detect line read wrong.
fn bring_up(board: &Board) -> Result<(), i64> {
    println!("[sd] slot power cycle at {} ms", time::now_ns() / 1_000_000);
    if !board.card_present() {
        println!("[sd] no card detected, probing anyway");
    }
    // A card the loader left powered keeps its 1.8 V UHS signaling, which
    // only a power cycle undoes: commands still pass, data never comes.
    // 239 ms off, hekate's T210 value for the rails to discharge (100 ms
    // failed after warm reboots).
    board.power_off();
    time::sleep_ms(239);
    board.power_on()?;
    libnx::clk::enable(clk::SDMMC1)
}

/// First block read before publishing: commands can pass while data
/// never comes (a card left at 1.8 V, a bad line). Other ways are then
/// tried, each reported; the first that reads wins.
fn recover(card: &mut Card, board: &Board) {
    let Err(e) = card.read_first() else {
        return;
    };
    println!(
        "[sd] first read failed ({}): {}",
        libnx::abi::err::name(e),
        card.describe()
    );
    println!("[sd] slot: {}", board.describe());
    // DAT0 alone reaching the card (hardware run 19): 1 bit in place,
    // since the power-cycled re-inits below failed there.
    let r = card.bus_1bit().and_then(|()| card.read_first());
    println!(
        "[sd] retry on a 1-bit bus in place: {}",
        match r {
            Ok(()) => "block 0 read",
            Err(e) => libnx::abi::err::name(e),
        }
    );
    if r.is_ok() {
        return;
    }
    // (1.8 V signaling, 4-bit bus, power-off ms before: none).
    let tries: [(bool, bool, u64); 3] = [(false, true, 2000), (false, false, 239), (true, true, 0)];
    for (v18, bus4, off_ms) in tries {
        let r = (|| {
            if off_ms != 0 {
                card.stop();
                board.power_off();
                time::sleep_ms(off_ms);
                board.power_on()?;
            }
            if v18 {
                board.io_1v8()?;
            }
            card.init(board, v18, bus4)?;
            card.read_first()
        })();
        println!(
            "[sd] retry at {} V, {}-bit bus, {} ms off: {}",
            if v18 { "1.8" } else { "3.3" },
            if bus4 { 4 } else { 1 },
            off_ms,
            match r {
                Ok(()) => "block 0 read",
                Err(e) => libnx::abi::err::name(e),
            }
        );
        if r.is_ok() {
            return;
        }
    }
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
    let dma = SharedMemory::new(PAGE, CONTIGUOUS | UNCACHED | DMA32).expect("sd: DMA page");
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
    // Touching SDMMC1 without its clock hangs the bus for everyone, so
    // the controller is only stopped once `bring_up` has clocked it.
    let mut clocked = false;
    let r = bring_up(&board).and_then(|()| {
        clocked = true;
        card.init(&board, false, true)
    });
    if let Err(e) = r {
        let why = alloc::format!("no usable card ({})", libnx::abi::err::name(e));
        drv::report("sd", &why);
        if clocked {
            card.stop();
        }
        board.power_off();
        loop {
            time::sleep_ms(3_600_000);
        }
    }
    recover(&mut card, &board);
    println!(
        "[sd] {} card of {} MiB ({} blocks), {}-bit bus, descriptors at {:#x}, ready at {} ms",
        if card.block_addressed {
            "SDHC/SDXC"
        } else {
            "SDSC"
        },
        card.blocks / 2048,
        card.blocks,
        card.bus_width(),
        card.dma_pa,
        time::now_ns() / 1_000_000
    );
    let ep = drv::publish("blk:sd").expect("sd: publish");
    let mut client: Option<Client> = None;
    let mut failures = 0u32;
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
                client = None;
                match Client::attach(ipc::received_handle(0)) {
                    Ok(c) => {
                        client = Some(c);
                        Msg::new(0, 0, 0)
                    }
                    Err(e) => {
                        println!("[sd] client buffer refused ({})", libnx::abi::err::name(e));
                        Msg::error(e)
                    }
                }
            }
            op @ (blk::READ | blk::WRITE) => {
                let (lba, n, off) = (m.mr[0], m.mr[1] as usize, m.mr[2] as usize);
                let Some(c) = client.as_ref() else {
                    return Some(Msg::error(-EINVAL));
                };
                // ADMA2 addresses: 4-byte aligned.
                if n > MAX_BLOCKS
                    || off % 4 != 0
                    || off.checked_add(n * BLOCK).is_none_or(|e| e > CLIENT_LEN)
                {
                    return Some(Msg::error(-EINVAL));
                }
                let r = card.transfer(lba, n, op == blk::READ, c.pa + off as u64);
                match r {
                    Ok(()) => Msg::new(0, 0, 0),
                    Err(e) => {
                        // The first failures in full: rails, pads, card.
                        failures += 1;
                        if failures <= 3 {
                            println!(
                                "[sd] failure {} at {} ms: {}",
                                failures,
                                time::now_ns() / 1_000_000,
                                card.describe()
                            );
                            println!("[sd] slot: {}", board.describe());
                            // MAX77620 LDO2 (card I/O rail): voltage and mode.
                            let ldo2 = libnx::i2c::I2c::connect()
                                .map(|i| (i.read_reg(5, 0x3C, 0x27), i.read_reg(5, 0x3C, 0x28)));
                            println!("[sd] PMIC LDO2 cfg, cfg2: {:?}", ldo2);
                        }
                        Msg::error(e)
                    }
                }
            }
            _ => Msg::error(-EINVAL),
        })
    })
}
