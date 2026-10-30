//! init: first system process; owns boot image and root capabilities.
//! Process manager and name service (publish badge vs lookup badge).

#![no_std]
#![no_main]

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;

use libnx::abi::err::{EINVAL, ENOENT, ENOSYS, EPERM};
use libnx::abi::proto::{console as con, emc, pm};
use libnx::abi::rights::{DUP, GRANT, MAP, SEND};
use libnx::abi::spawn::{self, HandleGrant};
use libnx::abi::{layout, slots, startup};
use libnx::bootfs::Bootfs;
use libnx::ipc::{self, Msg};
use libnx::process::{self, Spawn, Strings};
use libnx::{console, println, rt, sys};

const UART_A: u64 = 0x7000_6000;
const UART_A_IRQ: u32 = 32 + 36; // SPI 36

/// A driver or service started at boot, with its hardware resources.
/// They come from `slots::DEV_BASE`: register pages first, then
/// read-only ones, then IRQs, then sysctl.
struct Driver {
    name: &'static str,
    mmio: &'static [(u64, u64)],
    ro_mmio: &'static [(u64, u64)],
    irqs: &'static [u32],
    sysctl: bool,
    /// Badge of an extra console handle in `slots::TERMINAL` (0: none):
    /// the terminal driver's or the screen's.
    console: u64,
    prio: u32,
}

/// Start order: a service only depends on earlier ones
/// (and waits for them via `ns::wait_for`).
const DRIVERS: &[Driver] = &[
    // CAR, plus the PMC page to read power-partition status.
    Driver {
        name: "clk",
        // CAR, PMC, and the audio hub (its I2S blocks, for the MBIST
        // workaround after the AUD partition comes up).
        mmio: &[
            (0x6000_6000, 0x1000),
            (0x7000_E000, 0x1000),
            (0x702D_0000, 0x2000),
        ],
        ro_mmio: &[],
        irqs: &[],
        sysctl: false,
        console: 0,
        prio: 6,
    },
    Driver {
        name: "i2c",
        mmio: &[(0x7000_C000, 0x1000), (0x7000_D000, 0x1000)],
        ro_mmio: &[],
        // I2C1..6: SPI 38, 84, 92, 120, 53, 63.
        irqs: &[70, 116, 124, 152, 85, 95],
        sysctl: false,
        console: 0,
        prio: 6,
    },
    // APB_MISC read-only: the chip revision (the TMP451's Mariko offset).
    Driver {
        name: "power",
        mmio: &[],
        ro_mmio: &[(0x7000_0000, 0x1000)],
        irqs: &[],
        sysctl: true,
        console: 0,
        prio: 8,
    },
    // PWM (fan on channel 1), pinmux, GPIO (5V enable A5, PWM pad V4,
    // tachometer S7), PMC (GPIO pad power); APB_MISC read-only (chip
    // revision). IRQ: GPIO bank 4 (ports Q-T), SPI 55.
    Driver {
        name: "fan",
        mmio: &[
            (0x7000_A000, 0x1000),
            (0x7000_3000, 0x1000),
            (0x6000_D000, 0x1000),
            (0x7000_E000, 0x1000),
        ],
        ro_mmio: &[(0x7000_0000, 0x1000)],
        irqs: &[87],
        sysctl: false,
        console: 0,
        prio: 8,
    },
    // CAR (the CPU clock and PLLX registers only; clk never touches
    // them); APB_MISC (chip revision) and fuses (SKU, CPU speedo)
    // read-only. The CPU regulator goes through the i2c service.
    // DRAM clock: CAR (CLK_SOURCE_EMC, PLLM/PLLMB), MC and the EMC with its
    // two channels; hekate's trained tables, read-only, from the SECFW
    // carveout.
    Driver {
        name: "emc",
        mmio: &[
            (0x6000_6000, 0x1000),
            (0x7001_9000, 0x1000),
            (0x7001_B000, 0x5000),
        ],
        ro_mmio: &[(emc::TABLE_BASE, emc::TABLE_LEN)],
        irqs: &[],
        sysctl: false,
        console: 0,
        prio: 6,
    },
    Driver {
        name: "cpufreq",
        mmio: &[(0x6000_6000, 0x1000)],
        ro_mmio: &[(0x7000_0000, 0x1000), (0x7000_F000, 0x1000)],
        irqs: &[],
        sysctl: false,
        console: 0,
        prio: 8,
    },
    Driver {
        name: "display",
        // Display controller A, the PWM block (channel 0 dims the
        // backlight; the fan owns channel 1), DSI A (the panel), and for its
        // own bring-up (PLLD and the display clocks, panel reset and
        // rails on GPIO I and V through their masked registers and pads,
        // MIPI CAL); APB_MISC read-only (chip revision).
        mmio: &[
            (0x5420_0000, 0x40000),
            (0x7000_A000, 0x1000),
            (0x5430_0000, 0x40000),
            (0x6000_6000, 0x1000),
            (0x6000_D000, 0x1000),
            (0x7000_3000, 0x1000),
            (0x700E_3000, 0x1000),
        ],
        ro_mmio: &[(0x7000_0000, 0x1000)],
        irqs: &[105],
        sysctl: false,
        console: con::BADGE_SCREEN,
        prio: 10,
    },
    // Touchscreen (via the i2c service) and events from other drivers.
    // Touch interrupt on GPIO X1: GPIO controller, pinmux, GPIO bank 6
    // (ports U-X) interrupt, SPI 87.
    Driver {
        name: "input",
        mmio: &[(0x6000_D000, 0x1000), (0x7000_3000, 0x1000)],
        ro_mmio: &[],
        irqs: &[119],
        sysctl: false,
        console: 0,
        prio: 8,
    },
    // Joy-Cons on the rails: UART B and C (one page, UART-A's too), pinmux,
    // GPIO (rail detect H6 / E6, charge enables K3 / CC3); APB_MISC
    // read-only (chip revision). Clocks through clk, the 5V rail through fan.
    Driver {
        name: "joycon",
        mmio: &[
            (0x7000_6000, 0x1000),
            (0x7000_3000, 0x1000),
            (0x6000_D000, 0x1000),
        ],
        ro_mmio: &[(0x7000_0000, 0x1000)],
        irqs: &[],
        sysctl: false,
        console: 0,
        prio: 8,
    },
    // Audio: the codec on I2C1, I2S1 and the hub's ADMAIF / crossbar
    // (AHUB) and its DMA (ADMA), pinmux (DAP1, MCLK, codec LDO enable), GPIO (Z4). Clocks
    // and pad power through clk.
    Driver {
        name: "audio",
        mmio: &[
            (0x7000_3000, 0x1000),
            (0x6000_D000, 0x1000),
            (0x702D_0000, 0x2000),
            (0x702E_2000, 0x1000),
        ],
        ro_mmio: &[],
        irqs: &[],
        sysctl: false,
        console: 0,
        prio: 8,
    },
    // Started before `sd`: a driver that stalls the APB bus must not cost
    // us the console. Registers plus queue-head RAM; APB_MISC (chip
    // revision) and fuses (PHY trims) read-only. IRQ: SPI 20.
    Driver {
        name: "usbcon",
        mmio: &[(0x7D00_0000, 0x2000)],
        ro_mmio: &[(0x7000_0000, 0x1000), (0x7000_F000, 0x1000)],
        irqs: &[52],
        sysctl: false,
        console: con::BADGE_TERMINAL,
        prio: 6,
    },
    // SDMMC1, plus what powers the slot: APB_MISC (pad control, chip
    // revision), pinmux, GPIO (card detect/power) and PMC (I/O power).
    Driver {
        name: "sd",
        mmio: &[
            (0x700B_0000, 0x1000),
            (0x7000_0000, 0x1000),
            (0x7000_3000, 0x1000),
            (0x6000_D000, 0x1000),
            (0x7000_E000, 0x1000),
        ],
        ro_mmio: &[],
        irqs: &[46],
        sysctl: false,
        console: 0,
        prio: 6,
    },
    // The game card reader's ASIC: SDMMC2 inside SDMMC1's register page
    // (the window `gc` uses stops at 0x200), APB_MISC (pad control,
    // chip revision), pinmux (detect pad) and GPIO (detect S3, reset BB3).
    // Clocks and the rail go through clk and i2c; polled, no interrupt.
    Driver {
        name: "gc",
        mmio: &[
            (0x700B_0000, 0x1000),
            (0x7000_0000, 0x1000),
            (0x7000_3000, 0x1000),
            (0x6000_D000, 0x1000),
        ],
        ro_mmio: &[],
        irqs: &[],
        sysctl: false,
        console: 0,
        prio: 6,
    },
    Driver {
        name: "fs",
        mmio: &[],
        ro_mmio: &[],
        irqs: &[],
        sysctl: false,
        console: 0,
        prio: 8,
    },
    Driver {
        name: "usb",
        // XUSB: xHCI host registers.
        mmio: &[(0x7009_0000, 0x4000)],
        ro_mmio: &[],
        irqs: &[71],
        sysctl: false,
        console: 0,
        prio: 6,
    },
    // Wi-Fi: PCIe root complex (ports, PADS, AFI), the 4 KiB
    // configuration window, the pad control block holding the UPHY
    // lanes, pinmux / GPIO for the PEX and module pads, then the two
    // downstream windows the endpoint's BARs land in. IRQ: SPI 98 (AFI,
    // legacy INTx included).
    Driver {
        name: "wlan",
        mmio: &[
            (0x0100_0000, 0x4000),
            (0x0200_0000, 0x1000),
            (0x7009_F000, 0x1000),
            (0x7000_3000, 0x1000),
            (0x6000_D000, 0x1000),
            (0x1300_0000, 0x80_0000),
            (0x2000_0000, 0x40_0000),
        ],
        ro_mmio: &[],
        irqs: &[130],
        sysctl: false,
        console: 0,
        prio: 6,
    },
    // Bluetooth: UART-D (the UART page), pinmux and GPIO (BT_RST H4, wake
    // lines H3 / H5); IRQ: SPI 90 (UART-D). Clock through clk, firmware
    // and pairing keys through fs.
    Driver {
        name: "bluetooth",
        mmio: &[
            (0x7000_6000, 0x1000),
            (0x7000_3000, 0x1000),
            (0x6000_D000, 0x1000),
        ],
        ro_mmio: &[],
        irqs: &[122],
        sysctl: false,
        console: 0,
        prio: 8,
    },
    // GPU: BAR0 (priv registers), the start of BAR1 (the channel's
    // USERD); fuses (SKU, GPU speedo, PLL calibration) and APB_MISC
    // (chip revision) read-only. IRQs: SPI 157 (stall), 158 (nonstall).
    // Rail through i2c, clocks and clamps through clk.
    Driver {
        name: "gpu",
        mmio: &[(0x5700_0000, 0x100_0000), (0x5800_0000, 0x10_0000)],
        ro_mmio: &[(0x7000_F000, 0x1000), (0x7000_0000, 0x1000)],
        irqs: &[189, 190],
        sysctl: false,
        console: 0,
        prio: 8,
    },
    Driver {
        name: "net",
        mmio: &[],
        ro_mmio: &[],
        irqs: &[],
        sysctl: false,
        console: 0,
        prio: 8,
    },
];

struct Service {
    name: String,
    handle: u32,
    /// Process that published it: its names go when it is replaced.
    owner: u32,
}

struct State {
    fs: Bootfs,
    console: u32,
    /// Console handle of the shell and the programs it starts.
    session: u32,
    /// Console handles with the terminal and screen badges.
    terminal: u32,
    screen: u32,
    pm_app: u32,
    pm_srv: u32,
    services: Vec<Service>,
    /// Lookups waiting for a publication: (name, reply token).
    waiting: Vec<(String, u64)>,
    /// Running boot drivers: name and process handle (for `RESTART`).
    drivers: Vec<(&'static str, u32)>,
    /// The console shell, the one caller allowed to replace a driver.
    shell_pid: u32,
    /// Supervised drivers started again after a fault: how many times,
    /// and when last (ns).
    respawns: u32,
    last_respawn: u64,
}

/// Boot drivers started again when they exit: a fault killed them (the
/// Wi-Fi chip's bus errors do) and nothing else would bring them back.
const SUPERVISED: [&str; 1] = ["wlan"];
/// At least this long between two restarts, and no more than this many.
const RESPAWN_GAP_NS: u64 = 10_000_000_000;
const RESPAWN_MAX: u32 = 50;

libnx::entry!(main);

fn main() -> i32 {
    let boot_ms = libnx::time::now_ns() / 1_000_000;
    let fs =
        unsafe { Bootfs::new(layout::BOOTFS_BASE, rt::startup_arg()) }.expect("invalid boot image");
    println!("[init] {} programs in boot image", fs.count());
    println!("RESULT boot_to_init_ms {} ms", boot_ms);

    // --- Console driver: single entry point, one register page, one IRQ.
    let ep_console = sys::endpoint_create().expect("console endpoint");
    let uart = sys::mmio_derive(slots::INIT_MMIO_ROOT, UART_A, 0x1000).expect("uart mmio");
    let uart_irq = sys::irq_get(slots::INIT_IRQ_CONTROL, UART_A_IRQ).expect("uart irq");
    let console_drv = process::spawn_elf(&Spawn {
        elf: fs.find("console").expect("missing console"),
        name: "console",
        grants: &[
            HandleGrant {
                handle: ep_console,
                slot: slots::DRV_SERVICE,
            },
            HandleGrant {
                handle: uart,
                slot: slots::DRV_MMIO,
            },
            HandleGrant {
                handle: uart_irq,
                slot: slots::DRV_IRQ,
            },
            HandleGrant {
                handle: slots::INIT_KLOG,
                slot: slots::DRV_KLOG,
            },
        ],
        arg0: startup::IS_DRIVER,
        arg1: 0,
        prio: 4,
        cpu: None,
        strings: &Strings::new(&["console"], &[]),
    })
    .expect("spawn console");

    // init keeps send rights only: the driver alone may receive,
    // and clients learn of its death via EPIPE. Badged handles cannot
    // be re-minted: only the terminal driver gets the terminal badge.
    let console_send =
        sys::handle_dup(ep_console, SEND | GRANT | DUP, con::BADGE_CLIENT).expect("dup console");
    let console_term =
        sys::handle_dup(ep_console, SEND | GRANT, con::BADGE_TERMINAL).expect("dup console");
    let console_screen =
        sys::handle_dup(ep_console, SEND | GRANT, con::BADGE_SCREEN).expect("dup console");
    let console_session =
        sys::handle_dup(ep_console, SEND | GRANT | DUP, con::BADGE_SESSION).expect("dup console");
    for h in [ep_console, uart, uart_irq, console_drv] {
        let _ = sys::handle_close(h);
    }
    console::set(console_send);
    println!("[init] console driver started (UART-A, IRQ {})", UART_A_IRQ);

    // --- Process manager and name service.
    let ep_pm = sys::endpoint_create().expect("pm endpoint");
    let pm_app = sys::handle_dup(ep_pm, SEND | GRANT | DUP, pm::BADGE_APP).expect("dup pm");
    let pm_srv = sys::handle_dup(ep_pm, SEND | GRANT | DUP, pm::BADGE_SERVICE).expect("dup pm");
    let mut st = State {
        fs,
        console: console_send,
        session: console_session,
        terminal: console_term,
        screen: console_screen,
        pm_app,
        pm_srv,
        services: Vec::new(),
        waiting: Vec::new(),
        drivers: Vec::new(),
        shell_pid: 0,
        respawns: 0,
        last_respawn: 0,
    };

    for d in DRIVERS {
        let Some(elf) = st.fs.find(d.name) else {
            continue;
        };
        match start_driver(&st, d, elf) {
            Ok(h) => st.drivers.push((d.name, h)),
            Err(e) => println!(
                "[init] {}: failed to start ({})",
                d.name,
                libnx::abi::err::name(e)
            ),
        }
    }

    let shell = Strings::new(
        &["shell"],
        &[String::from("HOME=/"), String::from("PATH=/bin")],
    );
    match launch(&st, &shell, pm_app, &Stdio::CONSOLE, st.session) {
        Ok(h) => {
            st.shell_pid = sys::getpid(h).unwrap_or(0);
            let _ = sys::handle_close(h);
        }
        Err(e) => println!("[init] shell: error {}", libnx::abi::err::name(e)),
    }
    serve(&mut st, ep_pm)
}

/// Start a driver with its hardware capabilities.
fn start_driver(st: &State, d: &Driver, elf: &[u8]) -> Result<u32, i64> {
    let mut grants = Vec::new();
    let mut owned = Vec::new();
    grants.push(HandleGrant {
        handle: st.console,
        slot: slots::CONSOLE,
    });
    grants.push(HandleGrant {
        handle: st.pm_srv,
        slot: slots::PM,
    });
    let extra = match d.console {
        con::BADGE_TERMINAL => st.terminal,
        con::BADGE_SCREEN => st.screen,
        _ => 0,
    };
    if extra != 0 {
        grants.push(HandleGrant {
            handle: extra,
            slot: slots::TERMINAL,
        });
    }
    let mut slot = slots::DEV_BASE;
    let r = (|| {
        for &(base, len) in d.mmio {
            let h = sys::mmio_derive(slots::INIT_MMIO_ROOT, base, len)?;
            owned.push(h);
            grants.push(HandleGrant { handle: h, slot });
            slot += 1;
        }
        for &(base, len) in d.ro_mmio {
            // No WRITE right: mapped read-only.
            // DRAM (hekate's tables) is outside the MMIO root.
            let root = if base >= 0x8000_0000 {
                slots::INIT_FW_TABLES
            } else {
                slots::INIT_MMIO_ROOT
            };
            let h = sys::mmio_derive(root, base, len)?;
            let ro = sys::handle_dup(h, MAP | GRANT, 0);
            let _ = sys::handle_close(h);
            let ro = ro?;
            owned.push(ro);
            grants.push(HandleGrant { handle: ro, slot });
            slot += 1;
        }
        for &irq in d.irqs {
            let h = sys::irq_get(slots::INIT_IRQ_CONTROL, irq)?;
            owned.push(h);
            grants.push(HandleGrant { handle: h, slot });
            slot += 1;
        }
        if d.sysctl {
            grants.push(HandleGrant {
                handle: slots::INIT_SYSCTL,
                slot,
            });
        }
        process::spawn_elf(&Spawn {
            elf,
            name: d.name,
            grants: &grants,
            arg0: startup::HAS_CONSOLE | startup::HAS_PM | startup::IS_DRIVER,
            arg1: 0,
            prio: d.prio,
            cpu: None,
            strings: &Strings::new(&[d.name], &[]),
        })
    })();
    // Derived capabilities go to the driver: init keeps none.
    for h in owned {
        let _ = sys::handle_close(h);
    }
    r
}

/// `RESTART`: a boot driver replaced by a program from the filesystem,
/// same grants. The file is read first, so a bad path leaves the old
/// instance running; its names are dropped so the new one can publish.
fn restart(st: &mut State, name: &str, path: &str) -> Result<(), i64> {
    let d = DRIVERS.iter().find(|d| d.name == name).ok_or(-ENOENT)?;
    if !path.starts_with('/') {
        return Err(-EINVAL);
    }
    let elf = load(st, path)?;
    restart_elf(st, d, &elf, path)
}

/// `restart` with the program in hand.
fn restart_elf(st: &mut State, d: &'static Driver, elf: &[u8], path: &str) -> Result<(), i64> {
    let name = d.name;
    if let Some(k) = st.drivers.iter().position(|(n, _)| *n == name) {
        let (_, h) = st.drivers.swap_remove(k);
        let pid = sys::getpid(h).unwrap_or(0);
        let _ = sys::process_kill(h);
        // Its capabilities (IRQ line included) are gone once it is reaped.
        let _ = sys::process_wait_timeout(h, 2_000_000_000);
        let _ = sys::handle_close(h);
        st.services.retain(|s| {
            let keep = s.owner != pid;
            if !keep {
                let _ = sys::handle_close(s.handle);
            }
            keep
        });
    }
    let h = start_driver(st, d, elf)?;
    st.drivers.push((d.name, h));
    println!("[init] {} restarted from {}", name, path);
    Ok(())
}

/// A supervised driver that exited is started again, from `/bin/NAME` if
/// the card has one (the newer build), else from the boot image.
fn supervise(st: &mut State) {
    let now = libnx::time::now_ns();
    for name in SUPERVISED {
        let Some(&(_, h)) = st.drivers.iter().find(|(n, _)| *n == name) else {
            continue;
        };
        // Still running: the wait times out.
        let Ok(code) = sys::process_wait_timeout(h, 1_000) else {
            continue;
        };
        if st.respawns >= RESPAWN_MAX || now < st.last_respawn + RESPAWN_GAP_NS {
            continue;
        }
        st.respawns += 1;
        st.last_respawn = now;
        println!("[init] {} exited (code {}): starting it again", name, code);
        let Some(d) = DRIVERS.iter().find(|d| d.name == name) else {
            continue;
        };
        let card = alloc::format!("/bin/{}", name);
        let (elf, from) = match load(st, &card) {
            Ok(e) => (e, card),
            Err(_) => match load(st, name) {
                Ok(e) => (e, String::from(name)),
                Err(e) => {
                    println!("[init] {}: no program to start ({})", name, e);
                    continue;
                }
            },
        };
        if let Err(e) = restart_elf(st, d, &elf, &from) {
            println!("[init] {} did not start again: error {}", name, e);
        }
    }
}

/// Program executable: boot image, otherwise filesystem
/// (`/bin/NAME`, or the given path).
fn load(st: &State, name: &str) -> Result<Vec<u8>, i64> {
    if !name.contains('/') {
        if let Some(elf) = st.fs.find(name) {
            return Ok(elf.to_vec());
        }
    }
    let fs = st.services.iter().find(|s| s.name == "fs").ok_or(-ENOENT)?;
    libnx::fs::set_handle(fs.handle);
    let path = if name.contains('/') {
        String::from(name)
    } else {
        alloc::format!("/bin/{}", name)
    };
    libnx::fs::read(&path)
}

/// Application stdin/stdout/stderr: pipes from the requester, or
/// the console (plus flags for the program).
struct Stdio {
    handles: [Option<u32>; 3],
    flags: u64,
}

impl Stdio {
    const CONSOLE: Stdio = Stdio {
        handles: [None; 3],
        flags: 0,
    };

    /// Pipes passed with a SPAWN request (stdin, stdout, stderr).
    fn from_request(flags: u64, msg: &Msg) -> Stdio {
        let mut k = 0;
        let mut handles = [None; 3];
        for (i, bit) in [spawn::STDIN_PIPE, spawn::STDOUT_PIPE, spawn::STDERR_PIPE]
            .into_iter()
            .enumerate()
        {
            if flags & bit != 0 && k < msg.handles() {
                handles[i] = Some(ipc::received_handle(k));
                k += 1;
            }
        }
        let mut f = 0;
        for (i, bit) in [
            startup::STDIN_PIPE,
            startup::STDOUT_PIPE,
            startup::STDERR_PIPE,
        ]
        .into_iter()
        .enumerate()
        {
            if handles[i].is_some() {
                f |= bit;
            }
        }
        for (req, start) in [
            (spawn::STDIN_NULL, startup::STDIN_NULL),
            (spawn::STDOUT_NULL, startup::STDOUT_NULL),
            (spawn::STDERR_NULL, startup::STDERR_NULL),
        ] {
            if flags & req != 0 {
                f |= start;
            }
        }
        Stdio { handles, flags: f }
    }

    /// Init keeps no pipes: the program received its copy.
    fn close(&self) {
        for h in self.handles.iter().flatten() {
            let _ = sys::handle_close(*h);
        }
    }
}

/// Launch an application with standard capabilities: `console` (or
/// pipes) and manager (application badge).
fn launch(st: &State, strings: &Strings, pm: u32, io: &Stdio, console: u32) -> Result<u32, i64> {
    let path = strings.program();
    let elf = load(st, path)?;
    let name = path.rsplit('/').next().unwrap_or(path);
    let [stdin, stdout, stderr] = io.handles.map(|h| h.unwrap_or(console));
    // Only kernel-owned spawn flags (debugger stop, tied to the handle).
    let mut strings = strings.clone();
    strings.flags &= (spawn::SUSPENDED | spawn::KILL_ON_CLOSE) as u32;
    process::spawn_elf(&Spawn {
        elf: &elf,
        name,
        grants: &[
            HandleGrant {
                handle: stdout,
                slot: slots::CONSOLE,
            },
            HandleGrant {
                handle: pm,
                slot: slots::PM,
            },
            HandleGrant {
                handle: stdin,
                slot: slots::STDIN,
            },
            HandleGrant {
                handle: stderr,
                slot: slots::STDERR,
            },
        ],
        arg0: startup::HAS_CONSOLE | startup::HAS_PM | startup::HAS_STDIO | io.flags,
        arg1: 0,
        prio: 0,
        cpu: None,
        strings: &strings,
    })
}

/// Reply carrying a handle (init then closes its copy).
fn reply_handle(h: u32) -> Msg {
    ipc::set_send_handle(0, h);
    Msg::new(0, 0, 1)
}

fn serve(st: &mut State, ep: u32) -> ! {
    // A timer thread asks, every two seconds, for the supervised drivers
    // to be looked at (the loop below only wakes for requests).
    let _ = libnx::thread::spawn(move || loop {
        libnx::time::sleep_ms(2_000);
        let _ = ipc::call(ep, &Msg::new(pm::SUPERVISE, 0, 0));
    });
    let mut rx = ipc::recv(ep).expect("pm recv");
    let mut buf = alloc::vec![0u8; ipc::MAX_BYTES];
    loop {
        let mut to_close = 0u32;
        let mut deferred = false;
        let reply = match rx.msg.label() {
            pm::SPAWN => {
                let n = ipc::unpack(&rx.msg, &mut buf);
                let r = match Strings::decode(&buf[..n]) {
                    Some(s) => {
                        let io = Stdio::from_request(s.flags as u64, &rx.msg);
                        // Programs started by the shell's: its session.
                        let console = if rx.badge == pm::BADGE_APP {
                            st.session
                        } else {
                            st.console
                        };
                        let r = launch(st, &s, st.pm_app, &io, console);
                        io.close();
                        r
                    }
                    None => Err(-EINVAL),
                };
                match r {
                    Ok(h) => {
                        to_close = h;
                        reply_handle(h)
                    }
                    Err(e) => Msg::error(e),
                }
            }
            pm::LIST => {
                let mut names = Vec::new();
                for k in 0..st.fs.count() {
                    let n = st.fs.name(k);
                    if matches!(n, "init" | "console") || DRIVERS.iter().any(|d| d.name == n) {
                        continue;
                    }
                    if !names.is_empty() {
                        names.push(b' ');
                    }
                    names.extend_from_slice(n.as_bytes());
                }
                ipc::pack(0, &names)
            }
            pm::REGISTER => {
                let n = ipc::unpack(&rx.msg, &mut buf);
                let name = String::from(core::str::from_utf8(&buf[..n]).unwrap_or(""));
                let h = if rx.msg.handles() > 0 {
                    ipc::received_handle(0)
                } else {
                    0
                };
                if rx.badge != pm::BADGE_SERVICE {
                    if h != 0 {
                        let _ = sys::handle_close(h);
                    }
                    Msg::error(-EPERM)
                } else if h == 0 || name.is_empty() || st.services.iter().any(|s| s.name == name) {
                    if h != 0 {
                        let _ = sys::handle_close(h);
                    }
                    Msg::error(-EINVAL)
                } else {
                    // Pending lookups for this name get their reply.
                    let mut k = 0;
                    while k < st.waiting.len() {
                        if st.waiting[k].0 == name {
                            let (_, tok) = st.waiting.swap_remove(k);
                            if let Ok(d) = sys::handle_dup(h, SEND | GRANT | DUP, 0) {
                                let _ = ipc::reply_to(tok, &reply_handle(d));
                                let _ = sys::handle_close(d);
                            }
                        } else {
                            k += 1;
                        }
                    }
                    st.services.push(Service {
                        name,
                        handle: h,
                        owner: rx.pid,
                    });
                    Msg::new(0, 0, 0)
                }
            }
            label @ (pm::LOOKUP | pm::LOOKUP_WAIT) => {
                let n = ipc::unpack(&rx.msg, &mut buf);
                let name = core::str::from_utf8(&buf[..n]).unwrap_or("");
                match st.services.iter().find(|s| s.name == name) {
                    Some(s) => match sys::handle_dup(s.handle, SEND | GRANT | DUP, 0) {
                        Ok(d) => {
                            to_close = d;
                            reply_handle(d)
                        }
                        Err(e) => Msg::error(e),
                    },
                    None if label == pm::LOOKUP_WAIT => {
                        st.waiting.push((String::from(name), rx.token));
                        deferred = true;
                        Msg::default()
                    }
                    None => Msg::error(-ENOENT),
                }
            }
            pm::SERVICES => {
                let mut names = Vec::new();
                for s in &st.services {
                    if !names.is_empty() {
                        names.push(b' ');
                    }
                    names.extend_from_slice(s.name.as_bytes());
                }
                ipc::pack(0, &names)
            }
            // Only init's own timer thread asks for it.
            pm::SUPERVISE if sys::getpid(0).is_ok_and(|p| p != rx.pid) => Msg::error(-EPERM),
            pm::SUPERVISE => {
                supervise(st);
                Msg::new(0, 0, 0)
            }
            pm::RESTART if rx.pid != st.shell_pid || st.shell_pid == 0 => Msg::error(-EPERM),
            pm::RESTART => {
                let n = ipc::unpack(&rx.msg, &mut buf);
                let req = core::str::from_utf8(&buf[..n]).unwrap_or("");
                match req.split_once(' ') {
                    Some((name, path)) => match restart(st, name, path) {
                        Ok(()) => Msg::new(0, 0, 0),
                        Err(e) => Msg::error(e),
                    },
                    None => Msg::error(-EINVAL),
                }
            }
            _ => Msg::error(-ENOSYS),
        };
        if deferred {
            rx = ipc::recv(ep).expect("pm recv");
            continue;
        }
        if to_close != 0 {
            // A handle left with the reply: close our copy now.
            let _ = ipc::reply(&reply);
            let _ = sys::handle_close(to_close);
            rx = ipc::recv(ep).expect("pm recv");
            continue;
        }
        rx = match ipc::reply_recv(ep, &reply) {
            Ok(r) => r,
            Err(_) => ipc::recv(ep).expect("pm recv"),
        };
    }
}
