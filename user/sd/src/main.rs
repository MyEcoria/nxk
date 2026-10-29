//! sd: SDMMC1 card (SDHCI); 512-byte ADMA2 blocks via `blk` protocol,
//! straight into the client's buffer. The slot and card logic is the
//! `nxk-sdcard` crate (shared with nxk-ipl); this service powers the
//! card's rail through `power`, waits on the interrupt and serves blocks.

#![no_std]
#![no_main]

extern crate alloc;

use core::fmt;

use libnx::abi::err::EINVAL;
use libnx::abi::prot;
use libnx::abi::proto::{blk, clk, power};
use libnx::abi::shm::{CONTIGUOUS, DMA32, UNCACHED};
use libnx::drv::{self, Irq, Mmio};
use libnx::ipc::{self, Msg};
use libnx::mem::SharedMemory;
use libnx::{ns, println, sys, time};
use nxk_sdcard::board::{self, Board, Power};
use nxk_sdcard::{Card, Line, Regs, BLOCK, MAX_BLOCKS, PAGE, SDMMC1};

/// Client buffer size (`blk::ATTACH`).
const CLIENT_LEN: usize = MAX_BLOCKS * BLOCK;

/// A mapped register page.
struct Page(Mmio);

impl Regs for Page {
    fn r8(&self, off: usize) -> u8 {
        self.0.r8(off)
    }
    fn w8(&self, off: usize, v: u8) {
        self.0.w8(off, v)
    }
    fn r16(&self, off: usize) -> u16 {
        self.0.r16(off)
    }
    fn w16(&self, off: usize, v: u16) {
        self.0.w16(off, v)
    }
    fn r32(&self, off: usize) -> u32 {
        self.0.r32(off)
    }
    fn w32(&self, off: usize, v: u32) {
        self.0.w32(off, v)
    }
}

/// Time, the controller's interrupt and the console.
struct Host {
    irq: Irq,
}

impl nxk_sdcard::Host for Host {
    fn now_us(&self) -> u64 {
        time::now_ns() / 1000
    }

    fn sleep_us(&self, us: u64) {
        if us >= 1000 && us % 1000 == 0 {
            time::sleep_ms(us / 1000);
        } else {
            time::sleep_us(us);
        }
    }

    /// Transfers sleep on the interrupt, commands are polled (`time::poll`).
    fn wait(&self, timeout_us: u64, done: &dyn Fn() -> bool, xfer: bool) {
        if !xfer {
            time::poll(timeout_us, done);
            return;
        }
        let end = time::now_ns() + timeout_us * 1000;
        while !done() {
            let now = time::now_ns();
            if now > end {
                break;
            }
            self.irq.wait((end - now).max(1));
            self.irq.ack();
        }
    }

    /// Descriptors and buffer (uncached) in memory before the controller
    /// fetches them.
    fn dma_before(&self) {
        unsafe { core::arch::asm!("dsb st", options(nostack, preserves_flags)) };
    }

    fn log(&self, args: fmt::Arguments) {
        libnx::print!("{}", args);
    }
}

/// The card's rail through the `power` service, 1.8 V through `i2c`.
struct Rail {
    power: Option<u32>,
}

impl Power for Rail {
    fn sd_rail(&self, on: bool) -> nxk_sdcard::Result<()> {
        let ep = self.power.ok_or(-nxk_sdcard::ENODEV)?;
        let mut m = Msg::new(power::SD_POWER, 1, 0);
        m.mr[0] = on as u64;
        ipc::checked(ipc::call(ep, &m)?).map(|_| ())
    }

    fn ldo2_1v8(&self) -> nxk_sdcard::Result<()> {
        let i2c = libnx::i2c::I2c::connect()?;
        // MAX77620 LDO2: normal mode, 0.8 V + 20 x 50 mV.
        i2c.write_reg(5, 0x3C, 0x27, 0xC0 | 20)
    }
}

type SdBoard = Board<Page, Rail>;

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

fn map(slot: u32, phys: u64) -> Page {
    Page(Mmio::map(drv::dev_slot(slot), phys, 0x1000).expect("sd: registers"))
}

/// Slot power then controller clock. Card detect only reports: a card
/// that answers beats a detect line read wrong.
fn bring_up(board: &SdBoard, h: &Host) -> Result<(), i64> {
    println!("[sd] slot power cycle at {} ms", time::now_ns() / 1_000_000);
    if !board.card_present(h) {
        println!("[sd] no card detected, probing anyway");
    }
    // A card the loader left powered keeps its 1.8 V UHS signaling, which
    // only a power cycle undoes: commands still pass, data never comes.
    // 239 ms off, hekate's T210 value for the rails to discharge (100 ms
    // failed after warm reboots).
    board.power_off(h);
    time::sleep_ms(239);
    board.power_on(h)?;
    libnx::clk::enable(clk::SDMMC1)
}

fn main() -> i32 {
    let regs = map(0, SDMMC1);
    let board = Board::new(
        map(1, board::APB_MISC),
        map(2, board::PINMUX),
        map(3, board::GPIO),
        map(4, board::PMC_PAGE),
        Rail {
            power: ns::wait_for_timeout("power", 2_000_000_000).ok(),
        },
    );
    let irq = Irq::new(drv::dev_slot(5)).expect("sd: interrupt");
    let dma = SharedMemory::new(PAGE, CONTIGUOUS | UNCACHED | DMA32).expect("sd: DMA page");
    let dma_pa = dma.paddr(0).expect("sd: DMA address");
    let mut card = Card::new(regs, Host { irq }, dma.as_ptr() as *mut u32, dma_pa);
    // Touching SDMMC1 without its clock hangs the bus for everyone, so
    // the controller is only stopped once `bring_up` has clocked it.
    let mut clocked = false;
    let r = bring_up(&board, &card.host).and_then(|()| {
        clocked = true;
        card.init(&board, false, true)
    });
    if let Err(e) = r {
        let why = alloc::format!("no usable card ({})", libnx::abi::err::name(e));
        drv::report("sd", &why);
        if clocked {
            card.stop();
        }
        board.power_off(&card.host);
        loop {
            time::sleep_ms(3_600_000);
        }
    }
    let _ = nxk_sdcard::recover(&mut card, &board);
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
        card.dma_pa(),
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
                            let mut line = Line::new();
                            let _ = card.describe(&mut line);
                            println!(
                                "[sd] failure {} at {} ms: {}",
                                failures,
                                time::now_ns() / 1_000_000,
                                line.as_str()
                            );
                            let mut line = Line::new();
                            let _ = board.describe(&mut line);
                            println!("[sd] slot: {}", line.as_str());
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
