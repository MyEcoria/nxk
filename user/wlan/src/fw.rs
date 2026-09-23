//! Firmware boot on the BCM4356 (`brcmf_pcie_download_fw_nvram`): ARM
//! halted, image copied to its TCM through BAR2, NVRAM at the top of
//! RAM, ARM restarted; the firmware then publishes its shared area.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use libnx::drv::Mmio;
use libnx::time;

use crate::chip::{self, Backplane, Core};

/// Oldest to newest shared-area layout the host side understands.
const SHARED_VERSIONS: core::ops::RangeInclusive<u32> = 5..=7;
const SHARED_CONSOLE: u32 = 20;
// Firmware console: ring buffer address, size and write index.
const CONSOLE_BUF: u32 = 8;
const CONSOLE_SIZE: u32 = 12;
const CONSOLE_WRITE: u32 = 16;
/// How long the firmware gets to publish its shared area.
const BOOT_TIMEOUT_MS: u64 = 5000;

/// The running firmware: where its shared area and console live.
pub struct Up {
    pub shared: u32,
    pub flags: u32,
    pub console: u32,
}

/// NVRAM text to the chip's format (`brcmf_fw_nvram_strip`): each
/// `key=value` NUL-terminated, padded to a word, then a length token.
pub fn nvram(text: &[u8]) -> Vec<u8> {
    // Keys and values: printable, `#` starts a comment.
    let nv = |c: u8| (0x20..0x7F).contains(&c) && c != b'#';
    let mut out = Vec::with_capacity(text.len() + 32);
    let mut boardrev = false;
    for line in text.split(|&c| c == b'\n') {
        let mut i = 0;
        while i < line.len() {
            let c = line[i];
            if c == b'#' {
                break;
            }
            if c == b' ' || !nv(c) {
                i += 1;
                continue;
            }
            let start = i;
            while i < line.len() && nv(line[i]) && line[i] != b'=' && line[i] != b' ' {
                i += 1;
            }
            if i == line.len() || line[i] != b'=' {
                break; // no `=` after the key: the rest of the line is skipped
            }
            while i < line.len() && nv(line[i]) {
                i += 1;
            }
            let entry = &line[start..i];
            if entry.starts_with(b"RAW1") {
                break;
            }
            boardrev |= entry.starts_with(b"boardrev=");
            out.extend_from_slice(entry);
            out.push(0);
        }
    }
    if !boardrev {
        out.extend_from_slice(b"boardrev=0xff\0");
    }
    out.resize((out.len() + 1).next_multiple_of(4), 0);
    let words = (out.len() / 4) as u32;
    out.extend_from_slice(&((!words << 16) | (words & 0xFFFF)).to_le_bytes());
    out
}

/// Bytes into TCM, a word at a time (the tail padded with zeros).
fn copy(tcm: &Mmio, addr: u32, data: &[u8]) {
    for (k, w) in data.chunks(4).enumerate() {
        let mut b = [0u8; 4];
        b[..w.len()].copy_from_slice(w);
        tcm.w32(addr as usize + 4 * k, u32::from_le_bytes(b));
    }
}

/// Boot `image` with `nvram` (already converted); the step that failed
/// otherwise.
pub fn boot(
    bp: &Backplane,
    tcm: &Mmio,
    cores: &[Core],
    image: &[u8],
    nvram: &[u8],
) -> Result<Up, String> {
    let arm = chip::find(cores, chip::CORE_ARM_CR4).ok_or("no ARM CR4 core")?;
    let pcie2 = chip::find(cores, chip::CORE_PCIE2).ok_or("no PCIe2 core")?;
    chip::set_passive(bp, cores);
    chip::pcie2_quiet(bp, &pcie2);
    let (base, size) = (chip::RAMBASE_4356, chip::ram_size(bp, &arm));
    if image.len() < 4 || image.len() + nvram.len() > size as usize {
        return Err(format!(
            "{} + {} bytes do not fit {} of RAM",
            image.len(),
            nvram.len(),
            size
        ));
    }
    let top = base + size - 4;
    copy(tcm, base, image);
    // The last RAM word ends the NVRAM; the firmware overwrites it with
    // its shared area's address once it runs.
    tcm.w32(top as usize, 0);
    copy(tcm, base + size - nvram.len() as u32, nvram);
    let token = tcm.r32(top as usize);
    let entry = u32::from_le_bytes([image[0], image[1], image[2], image[3]]);
    tcm.w32(0, entry);
    chip::set_active(bp, &arm);

    let deadline = time::now_ns() + BOOT_TIMEOUT_MS * 1_000_000;
    let shared = loop {
        time::sleep_ms(50);
        let v = tcm.r32(top as usize);
        if v != token {
            break v;
        }
        if time::now_ns() > deadline {
            return Err(format!(
                "firmware silent after {} ms ({} KiB RAM)",
                BOOT_TIMEOUT_MS,
                size / 1024
            ));
        }
    };
    if !(base..base + size).contains(&shared) {
        return Err(format!("shared area at {:#x}, outside RAM", shared));
    }
    let flags = tcm.r32(shared as usize);
    let console = tcm.r32((shared + SHARED_CONSOLE) as usize);
    if !SHARED_VERSIONS.contains(&(flags & 0xFF)) {
        return Err(format!(
            "shared area version {} (flags {:#x})",
            flags & 0xFF,
            flags
        ));
    }
    Ok(Up {
        shared,
        flags,
        console,
    })
}

/// Last `max` printable bytes of the firmware console, lines joined.
pub fn console_tail(tcm: &Mmio, console: u32, max: usize) -> String {
    let buf = tcm.r32((console + CONSOLE_BUF) as usize);
    let size = tcm.r32((console + CONSOLE_SIZE) as usize);
    let write = tcm.r32((console + CONSOLE_WRITE) as usize);
    if size == 0 || size > 0x1_0000 || write >= size {
        return format!(
            "console unreadable (buf {:#x}, size {}, index {})",
            buf, size, write
        );
    }
    let mut text = Vec::new();
    for k in 0..size {
        let c = tcm.r8((buf + (write + k) % size) as usize);
        match c {
            b'\n' => text.extend_from_slice(b" | "),
            0x20..=0x7E => text.push(c),
            _ => {}
        }
    }
    let from = text.len().saturating_sub(max);
    String::from_utf8_lossy(&text[from..]).into_owned()
}
