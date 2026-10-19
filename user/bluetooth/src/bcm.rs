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
/// Speeds tried when the chip does not answer at 115200.
const PROBE: [u64; 4] = [3_000_000, 1_000_000, 921_600, 460_800];
const FIRMWARE_DIR: &str = "/firmware/brcm";
/// `hwboot/nxfw-extract.py bluetooth` names it so.
const FIRMWARE: &str = "BCM4356A3.hcd";
/// Linux `btbcm_patchram`: after the minidriver, after the launch.
const MINIDRIVER_MS: u64 = 50;
const LAUNCH_MS: u64 = 500;
/// Event mask: everything up to Secure Simple Pairing (Linux default).
const EVENT_MASK: [u8; 8] = [0xFF, 0xFF, 0xFB, 0xFF, 0x07, 0xF8, 0xBF, 0x3D];
/// Class of device: computer, handheld.
const CLASS: [u8; 3] = [0x10, 0x01, 0x00];
/// Role switch and sniff allowed (link policy settings).
const LINK_POLICY: u16 = 0x0005;
/// Page timeout: 0x8000 slots, 20.48 s (a controller alternates its
/// scans in sync mode).
const PAGE_TIMEOUT: u16 = 0x8000;
/// Connection accept timeout: 0x7D00 slots, 20 s, as Linux sets it
/// (the 5 s default ends a controller's connection with 0x10).
const ACCEPT_TIMEOUT: u16 = 0x7D00;
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

/// HCI Reset, retried: the chip takes a moment after power-on. A
/// Hardware Error ends it: only a power cycle clears that.
fn reset(hci: &mut Hci, tries: u32) -> Result<(), String> {
    let mut last = String::new();
    let errors = hci.hw_errors;
    for _ in 0..tries {
        hci.flush();
        match hci.call(hci::OP_RESET, &[], 500) {
            Ok(_) => return Ok(()),
            Err(e) => last = e,
        }
        if hci.hw_errors > errors {
            return Err(format!(
                "HCI reset: controller hardware error at {} baud",
                hci.uart.baud
            ));
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
pub fn bring_up(hci: &mut Hci, addr: Addr, wake: &dyn Fn(bool)) -> Result<Chip, String> {
    hci.uart.set_baud(BAUD_INIT);
    let errors = hci.hw_errors;
    if let Err(e) = reset(hci, 6) {
        // Events read whole: the speed is right, the core needs a power cycle.
        if hci.hw_errors > errors {
            return Err(e);
        }
        // A chip left at another speed: the 48 MHz divisors.
        let found = PROBE.iter().copied().find(|&b| {
            hci.uart.set_baud(b);
            reset(hci, 2).is_ok()
        });
        match found {
            Some(b) => println!("[bt] no answer at 115200, the chip answers at {} baud", b),
            None => {
                hci.uart.set_baud(BAUD_INIT);
                return Err(e);
            }
        }
    }
    let (_, subver) = version(hci)?;
    let name = local_name(hci);
    println!("[bt] {} answers, LMP subversion {:#06x}", name, subver);
    let fast = go_fast(hci);

    let firmware = match firmware_file(&name) {
        Some((file, data)) => {
            let n = download(hci, &data)?;
            // The patch restarts the chip at its default speed, or at
            // one the file sets: each is tried.
            let _ = fast;
            let speeds = [BAUD_INIT, BAUD_FAST, 1_000_000, 921_600, 460_800];
            let mut at = None;
            'found: for level in [false, true] {
                wake(level);
                time::sleep_ms(20);
                for b in speeds {
                    hci.uart.set_baud(b);
                    if reset(hci, 2).is_ok() {
                        at = Some((b, level));
                        break 'found;
                    }
                }
            }
            match at {
                Some((b, w)) => println!(
                    "[bt] patched firmware answers at {} baud, BT_WAKE {}",
                    b,
                    if w { "high" } else { "low" }
                ),
                None => {
                    return Err(format!(
                        "no answer after {} ({} records) at 115200, 3M, 1M, 921600 or 460800 baud",
                        file, n
                    ))
                }
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
        hci::OP_WRITE_CONN_ACCEPT_TIMEOUT,
        &[ACCEPT_TIMEOUT as u8, (ACCEPT_TIMEOUT >> 8) as u8],
        500,
    )?;
    hci.call(
        hci::OP_WRITE_PAGE_TIMEOUT,
        &[PAGE_TIMEOUT as u8, (PAGE_TIMEOUT >> 8) as u8],
        500,
    )?;
    if let Ok(t) = hci.call(hci::OP_READ_PAGE_TIMEOUT, &[], 500) {
        if t.len() >= 2 && le16(&t) != PAGE_TIMEOUT {
            println!(
                "[bt] page timeout {:#06x}, not {:#06x}",
                le16(&t),
                PAGE_TIMEOUT
            );
        }
    }
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
