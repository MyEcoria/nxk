//! BCM4356 Bluetooth bring-up, as Linux `hci_bcm.c` / `btbcm.c` do it:
//! reset at 115200 baud, 3 Mbaud (`0xFC18`), the patch RAM download
//! (`BCM4356A3.hcd`: minidriver, then the file's own HCI commands, the
//! last one launching it), reset and 3 Mbaud again, then the address
//! and the host settings classic HID needs.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use libnx::{fs, println, time};

use crate::hci::{self, le16, Hci};
use crate::store::{addr_str, Addr};

/// Company identifier of Broadcom (Bluetooth assigned numbers).
const BROADCOM: u16 = 0x000F;
pub const BAUD_INIT: u64 = 115_200;
pub const BAUD_FAST: u64 = 3_000_000;
const FIRMWARE_DIR: &str = "/firmware/brcm";
/// `hwboot/nxfw-extract.py bluetooth` names it so.
const FIRMWARE: &str = "BCM4356A3.hcd";
/// Linux `btbcm_patchram`: after the minidriver, after the launch.
const MINIDRIVER_MS: u64 = 50;
const LAUNCH_MS: u64 = 250;
/// Event mask: everything up to Secure Simple Pairing (Linux default).
const EVENT_MASK: [u8; 8] = [0xFF, 0xFF, 0xFB, 0xFF, 0x07, 0xF8, 0xBF, 0x3D];
/// Class of device: computer, handheld.
const CLASS: [u8; 3] = [0x10, 0x01, 0x00];
/// Role switch and sniff allowed (link policy settings).
const LINK_POLICY: u16 = 0x0005;
/// Page timeout: 0x2000 slots, 5.12 s.
const PAGE_TIMEOUT: u16 = 0x2000;
/// Page scan only: paired controllers reconnect, nobody finds us.
pub const SCAN_PAGE: u8 = 0x02;

pub struct Chip {
    pub name: String,
    pub lmp_subver: u16,
    /// Patch file loaded, or why not.
    pub firmware: String,
    pub addr: Addr,
    pub acl_mtu: u16,
    pub acl_pkts: u16,
    pub baud: u64,
}

impl Chip {
    pub fn describe(&self) -> String {
        format!(
            "{} (LMP subversion {:#06x}), {}, {} at {} baud",
            self.name,
            self.lmp_subver,
            self.firmware,
            addr_str(&self.addr),
            self.baud
        )
    }
}

/// HCI Reset, retried: the chip takes a moment after power-on.
fn reset(hci: &mut Hci, tries: u32) -> Result<(), String> {
    let mut last = String::new();
    for _ in 0..tries {
        hci.flush();
        match hci.call(hci::OP_RESET, &[], 500) {
            Ok(_) => return Ok(()),
            Err(e) => last = e,
        }
    }
    Err(format!("HCI reset: {}", last))
}

/// Chip and host UART to 3 Mbaud; false: both stay where they were.
fn go_fast(hci: &mut Hci) -> bool {
    let b = BAUD_FAST as u32;
    let p = [
        0,
        0,
        b as u8,
        (b >> 8) as u8,
        (b >> 16) as u8,
        (b >> 24) as u8,
    ];
    if hci.call(hci::OP_BCM_UART_BAUD, &p, 500).is_err() {
        return false;
    }
    hci.uart.set_baud(BAUD_FAST);
    hci.flush();
    time::sleep_ms(10);
    if hci.call(hci::OP_READ_VERSION, &[], 500).is_ok() {
        return true;
    }
    println!("[bt] no answer at 3 Mbaud, back to 115200");
    hci.uart.set_baud(BAUD_INIT);
    hci.flush();
    false
}

fn version(hci: &mut Hci) -> Result<(u16, u16), String> {
    let v = hci.call(hci::OP_READ_VERSION, &[], 500)?;
    if v.len() < 8 {
        return Err(String::from("short local version"));
    }
    Ok((le16(&v[4..6]), le16(&v[6..8])))
}

fn local_name(hci: &mut Hci) -> String {
    match hci.call(hci::OP_READ_LOCAL_NAME, &[], 500) {
        Ok(n) => {
            let end = n.iter().position(|&b| b == 0).unwrap_or(n.len());
            String::from_utf8_lossy(&n[..end]).into_owned()
        }
        Err(_) => String::from("unnamed"),
    }
}

/// The patch file: `BCM4356A3.hcd`, else the one named after the chip,
/// else any `.hcd` in the firmware directory.
fn firmware_file(chip_name: &str) -> Option<(String, Vec<u8>)> {
    let mut names = alloc::vec![String::from(FIRMWARE)];
    if let Some(n) = chip_name.split_whitespace().next() {
        names.push(format!("{}.hcd", n));
    }
    if let Ok(dir) = fs::read_dir(FIRMWARE_DIR) {
        names.extend(
            dir.into_iter()
                .map(|e| e.name)
                .filter(|n| n.ends_with(".hcd")),
        );
    }
    names.into_iter().find_map(|n| {
        fs::read(&format!("{}/{}", FIRMWARE_DIR, n))
            .ok()
            .map(|d| (n, d))
    })
}

/// `btbcm_patchram`: the file is a list of HCI commands (opcode, length,
/// parameters), sent one after the other.
fn download(hci: &mut Hci, data: &[u8]) -> Result<usize, String> {
    hci.call(hci::OP_BCM_DOWNLOAD_MINIDRIVER, &[], 1000)?;
    time::sleep_ms(MINIDRIVER_MS);
    let (mut at, mut n) = (0, 0);
    while at + 3 <= data.len() {
        let op = le16(&data[at..at + 2]);
        let len = data[at + 2] as usize;
        let p = data
            .get(at + 3..at + 3 + len)
            .ok_or_else(|| format!("patch file cut at byte {}", at))?;
        hci.call(op, p, 1000)
            .map_err(|e| format!("patch record {}: {}", n, e))?;
        at += 3 + len;
        n += 1;
    }
    time::sleep_ms(LAUNCH_MS);
    Ok(n)
}

/// Power-cycled chip to a configured adapter at `addr`.
pub fn bring_up(hci: &mut Hci, addr: Addr) -> Result<Chip, String> {
    hci.uart.set_baud(BAUD_INIT);
    reset(hci, 6)?;
    let (_, subver) = version(hci)?;
    let name = local_name(hci);
    println!("[bt] {} answers, LMP subversion {:#06x}", name, subver);
    let fast = go_fast(hci);

    let firmware = match firmware_file(&name) {
        Some((file, data)) => {
            let n = download(hci, &data)?;
            // The patch restarts the chip at its default speed.
            hci.uart.set_baud(BAUD_INIT);
            if reset(hci, 3).is_err() && fast {
                hci.uart.set_baud(BAUD_FAST);
                reset(hci, 2)?;
            }
            println!("[bt] {} loaded ({} records)", file, n);
            format!("{} loaded", file)
        }
        None => {
            println!("[bt] no {}/*.hcd: ROM firmware", FIRMWARE_DIR);
            String::from("ROM firmware (no patch file)")
        }
    };
    if hci.uart.baud != BAUD_FAST {
        go_fast(hci);
    }
    let (manufacturer, lmp_subver) = version(hci)?;
    if manufacturer != BROADCOM {
        println!("[bt] manufacturer {:#06x}, not Broadcom", manufacturer);
    }
    // Sleep mode off: the wake lines stay unused (Linux sends the same
    // without its low-power support).
    let _ = hci.call(hci::OP_BCM_SLEEP_MODE, &[0; 12], 500);

    let mut wire = addr;
    hci.call(hci::OP_BCM_WRITE_BDADDR, &wire, 500)?;
    let got = hci.call(hci::OP_READ_BDADDR, &[], 500)?;
    if got.len() >= 6 {
        wire.copy_from_slice(&got[..6]);
    }
    let b = hci.call(hci::OP_READ_BUFFER_SIZE, &[], 500)?;
    if b.len() < 7 {
        return Err(String::from("short buffer size"));
    }
    let (acl_mtu, acl_pkts) = (le16(&b[0..2]), le16(&b[3..5]));

    hci.call(hci::OP_SET_EVENT_MASK, &EVENT_MASK, 500)?;
    hci.call(hci::OP_WRITE_SSP_MODE, &[1], 500)?;
    // Extended results carry names; older controllers refuse it.
    let _ = hci.call(hci::OP_WRITE_INQUIRY_MODE, &[2], 500);
    hci.call(hci::OP_WRITE_CLASS, &CLASS, 500)?;
    let mut local = [0u8; 248];
    local[..3].copy_from_slice(b"nxk");
    hci.call(hci::OP_WRITE_LOCAL_NAME, &local, 500)?;
    hci.call(
        hci::OP_DEFAULT_LINK_POLICY,
        &[LINK_POLICY as u8, (LINK_POLICY >> 8) as u8],
        500,
    )?;
    hci.call(
        hci::OP_WRITE_PAGE_TIMEOUT,
        &[PAGE_TIMEOUT as u8, (PAGE_TIMEOUT >> 8) as u8],
        500,
    )?;
    hci.call(hci::OP_WRITE_SCAN_ENABLE, &[SCAN_PAGE], 500)?;
    hci.flush();
    Ok(Chip {
        name,
        lmp_subver,
        firmware,
        addr: wire,
        acl_mtu,
        acl_pkts,
        baud: hci.uart.baud,
    })
}
