//! sd : carte SD du lecteur (controleur SDMMC1, compatible SDHCI).
//!
//! Initialisation SD standard (CMD0, CMD8, ACMD41, CMD2/3/9/7, bus 4 bits),
//! puis lecture et ecriture par blocs de 512 octets en DMA ADMA2. Le tampon
//! DMA appartient au pilote (contigu, non cache, sous 4 GiB) : un client ne
//! donne jamais d'adresse physique, il echange les donnees par une memoire
//! partagee attachee au service (protocole `blk`).
//!
//! Sur la console, SDMMC1 demande aussi l'alimentation de la carte par le
//! PMIC et le reglage des pads : laisses au chargeur d'amorcage ici.

#![no_std]
#![no_main]

extern crate alloc;

use libnx::abi::err::{EINVAL, EIO, ENODEV, ETIMEDOUT};
use libnx::abi::proto::{blk, clk};
use libnx::abi::shm::{CONTIGUOUS, DMA32, UNCACHED};
use libnx::abi::prot;
use libnx::drv::{self, Irq, Mmio};
use libnx::ipc::{self, Msg};
use libnx::mem::SharedMemory;
use libnx::{ns, println, sys, time};

const SDMMC1: u64 = 0x700B_0000;

// Registres SDHCI.
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
const CAPABILITIES: usize = 0x40;
const ADMA_ADDRESS: usize = 0x58;
const VENDOR_CLOCK_CNTRL: usize = 0x100;

const INT_CMD_DONE: u32 = 1 << 0;
const INT_XFER_DONE: u32 = 1 << 1;
const INT_ERROR: u32 = 1 << 15;

const BLOCK: usize = 512;
/// Tampon DMA : une page de descripteurs puis 256 KiB de donnees.
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
    /// Adressage par blocs (SDHC/SDXC) ou par octets (SDSC).
    block_addressed: bool,
    blocks: u64,
}

impl Card {
    fn reset(&self, what: u8) {
        self.regs.w8(SOFTWARE_RESET, what);
        let end = time::now_ns() + 100_000_000;
        while self.regs.r8(SOFTWARE_RESET) & what != 0 && time::now_ns() < end {}
    }

    fn set_clock(&self, khz: u32) {
        self.regs.w16(CLOCK_CONTROL, 0);
        let base = ((self.regs.r32(CAPABILITIES) >> 8) & 0xFF).max(1) * 1000;
        let div = if khz >= base { 0 } else { base.div_ceil(2 * khz) };
        let v = ((div & 0xFF) << 8 | ((div >> 8) & 3) << 6 | 1) as u16;
        self.regs.w16(CLOCK_CONTROL, v);
        let end = time::now_ns() + 20_000_000;
        while self.regs.r16(CLOCK_CONTROL) & 2 == 0 && time::now_ns() < end {}
        self.regs.w16(CLOCK_CONTROL, v | 4);
    }

    /// Attend l'un des bits `mask` du registre d'etat ; l'interruption
    /// reveille le thread pour les transferts, sinon courte attente active.
    fn wait_int(&self, mask: u32, timeout_ms: u64) -> Result<u32, i64> {
        let end = time::now_ns() + timeout_ms * 1_000_000;
        loop {
            let st = self.regs.r32(INT_STATUS);
            if st & INT_ERROR != 0 {
                self.regs.w32(INT_STATUS, st);
                self.reset(0x6); // lignes CMD et DAT
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

    fn command(&self, idx: u8, arg: u32, rsp: Rsp, data: Option<(usize, bool)>) -> Result<[u32; 4], i64> {
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
            self.regs.w8(HOST_CONTROL, (self.regs.r8(HOST_CONTROL) & !0x18) | 0x10); // ADMA2 32 bits
            self.regs.w16(BLOCK_SIZE, BLOCK as u16 | 7 << 12);
            self.regs.w16(BLOCK_COUNT, n as u16);
            mode = 1 | 2; // DMA, compteur de blocs
            if n > 1 {
                mode |= 1 << 5 | 1 << 2; // plusieurs blocs, CMD12 automatique
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

    /// Descripteurs ADMA2 (32 bits) couvrant `len` octets du tampon.
    fn write_descriptors(&self, len: usize) {
        let d = self.dma.as_ptr() as *mut u32;
        let mut off = 0;
        let mut k = 0;
        while off < len {
            let n = (len - off).min(65536);
            let last = off + n >= len;
            let attr = 1 | 0x20 | if last { 2 } else { 0 }; // valide, transfert, fin
            unsafe {
                d.add(2 * k).write_volatile(((n as u32 & 0xFFFF) << 16) | attr);
                d.add(2 * k + 1).write_volatile((self.dma_pa + (DESC_BYTES + off) as u64) as u32);
            }
            off += n;
            k += 1;
        }
    }

    fn app_command(&self, idx: u8, arg: u32, rsp: Rsp) -> Result<[u32; 4], i64> {
        self.command(55, self.rca << 16, Rsp::R1, None)?;
        self.command(idx, arg, rsp, None)
    }

    fn init(&mut self) -> Result<(), i64> {
        if self.regs.r32(PRESENT_STATE) & (1 << 16) == 0 {
            return Err(-ENODEV); // pas de carte
        }
        self.regs.modify(VENDOR_CLOCK_CNTRL, 0, 1);
        self.reset(0x1);
        self.regs.w8(POWER_CONTROL, 0x0F); // 3,3 V
        self.set_clock(400);
        self.regs.w8(TIMEOUT_CONTROL, 0x0E);
        self.regs.w16(INT_ENABLE, 0xFFFF);
        self.regs.w16(INT_ENABLE + 2, 0xFFFF);
        self.regs.w16(INT_SIGNAL, (INT_XFER_DONE | INT_ERROR) as u16);
        self.regs.w16(INT_SIGNAL + 2, 0xFFFF);
        time::sleep_ms(2);

        self.command(0, 0, Rsp::None, None)?;
        let v2 = self.command(8, 0x1AA, Rsp::R1, None).map(|r| r[0] & 0xFFF == 0x1AA).unwrap_or(false);
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
        self.app_command(6, 2, Rsp::R1)?; // bus 4 bits
        self.regs.w8(HOST_CONTROL, self.regs.r8(HOST_CONTROL) | 2);
        if !self.block_addressed {
            self.command(16, BLOCK as u32, Rsp::R1, None)?;
        }
        self.set_clock(25_000);
        Ok(())
    }

    fn transfer(&self, lba: u64, n: usize, read: bool) -> Result<(), i64> {
        if n == 0 || n > MAX_BLOCKS || lba + n as u64 > self.blocks {
            return Err(-EINVAL);
        }
        let arg = if self.block_addressed { lba } else { lba * BLOCK as u64 } as u32;
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

fn main() -> i32 {
    let regs = Mmio::map(drv::dev_slot(0), SDMMC1, 0x1000).expect("sd : registres");
    let irq = Irq::new(drv::dev_slot(1)).expect("sd : interruption");
    if let Ok(c) = ns::wait_for_timeout("clk", 2_000_000_000) {
        let mut m = Msg::new(clk::ENABLE, 1, 0);
        m.mr[0] = clk::SDMMC1;
        let _ = ipc::call(c, &m);
        let _ = sys::handle_close(c);
    }
    let dma = SharedMemory::new(DESC_BYTES + MAX_BLOCKS * BLOCK, CONTIGUOUS | UNCACHED | DMA32).expect("sd : tampon DMA");
    let dma_pa = dma.paddr(0).expect("sd : adresse DMA");
    let mut card = Card { regs, irq, dma, dma_pa, rca: 0, block_addressed: false, blocks: 0 };
    if let Err(e) = card.init() {
        println!("[sd] pas de carte utilisable ({})", libnx::abi::err::name(e));
        loop {
            time::sleep_ms(3_600_000);
        }
    }
    println!(
        "[sd] carte {} de {} Mio ({} blocs)",
        if card.block_addressed { "SDHC/SDXC" } else { "SDSC" },
        card.blocks / 2048,
        card.blocks
    );
    let ep = drv::publish("blk:sd").expect("sd : publication");
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
                let Some(buf) = client.as_ref() else { return Some(Msg::error(-EINVAL)) };
                let len = n * BLOCK;
                if off.checked_add(len).is_none_or(|e| e > buf.len()) || n > MAX_BLOCKS {
                    return Some(Msg::error(-EINVAL));
                }
                let r = if op == blk::READ {
                    card.transfer(lba, n, true).map(|_| buf.as_mut_slice()[off..off + len].copy_from_slice(&card.data()[..len]))
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
