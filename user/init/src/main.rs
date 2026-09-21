//! init : premier processus du systeme.
//!
//! Recoit du noyau l'image de demarrage et les capacites racines (controle
//! des interruptions, plage des peripheriques, controle du systeme). Il en
//! tire, pour chaque pilote, exactement ce qu'il lui faut : quelques pages de
//! registres et quelques lignes d'interruption. Puis il sert de gestionnaire
//! de processus et de service de noms : les pilotes y publient leur
//! endpoint, les programmes le demandent par son nom.
//!
//! L'endpoint du gestionnaire est distribue avec deux insignes : les
//! services (lances ici) peuvent publier, les applications seulement
//! chercher. Un programme ne peut donc pas se faire passer pour « fs ».

#![no_std]
#![no_main]

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;

use libnx::abi::err::{EINVAL, ENOENT, ENOSYS, EPERM};
use libnx::abi::proto::pm;
use libnx::abi::rights::{DUP, GRANT, SEND};
use libnx::abi::spawn::{self, HandleGrant};
use libnx::abi::{layout, slots, startup};
use libnx::bootfs::Bootfs;
use libnx::ipc::{self, Msg};
use libnx::process::{self, Spawn, Strings};
use libnx::{console, println, rt, sys};

const UART_A: u64 = 0x7000_6000;
const UART_A_IRQ: u32 = 32 + 36; // SPI 36

/// Un pilote ou service lance au demarrage, avec ses ressources materielles.
/// Il les recoit a partir de `slots::DEV_BASE` : pages de registres d'abord,
/// puis interruptions, puis le controle du systeme s'il en a besoin.
struct Driver {
    name: &'static str,
    mmio: &'static [(u64, u64)],
    irqs: &'static [u32],
    sysctl: bool,
    prio: u32,
}

/// Ordre de lancement : un service ne depend que de ceux qui le precedent
/// (et les attend par `ns::wait_for`).
const DRIVERS: &[Driver] = &[
    Driver { name: "clk", mmio: &[(0x6000_6000, 0x1000)], irqs: &[], sysctl: false, prio: 6 },
    Driver {
        name: "i2c",
        mmio: &[(0x7000_C000, 0x1000), (0x7000_D000, 0x1000)],
        // I2C1..6 : SPI 38, 84, 92, 120, 53, 63.
        irqs: &[70, 116, 124, 152, 85, 95],
        sysctl: false,
        prio: 6,
    },
    Driver { name: "power", mmio: &[], irqs: &[], sysctl: true, prio: 8 },
    Driver { name: "sd", mmio: &[(0x700B_0000, 0x1000)], irqs: &[46], sysctl: false, prio: 6 },
    Driver { name: "fs", mmio: &[], irqs: &[], sysctl: false, prio: 8 },
    Driver {
        name: "display",
        mmio: &[(0x5420_0000, 0x40000)],
        irqs: &[105],
        sysctl: false,
        prio: 10,
    },
    // Ecran tactile (par le service i2c) et evenements des autres pilotes.
    Driver { name: "input", mmio: &[], irqs: &[], sysctl: false, prio: 8 },
    Driver {
        name: "usb",
        // XUSB : registres de l'hote xHCI.
        mmio: &[(0x7009_0000, 0x4000)],
        irqs: &[71],
        sysctl: false,
        prio: 6,
    },
    Driver { name: "net", mmio: &[], irqs: &[], sysctl: false, prio: 8 },
];

struct Service {
    name: String,
    handle: u32,
}

struct State {
    fs: Bootfs,
    console: u32,
    pm_app: u32,
    pm_srv: u32,
    services: Vec<Service>,
    /// Recherches en attente d'une publication : (nom, jeton de reponse).
    waiting: Vec<(String, u64)>,
}

libnx::entry!(main);

fn main() -> i32 {
    let boot_ms = libnx::time::now_ns() / 1_000_000;
    let fs = unsafe { Bootfs::new(layout::BOOTFS_BASE, rt::startup_arg()) }.expect("image de demarrage invalide");
    println!("[init] {} programmes dans l'image de demarrage", fs.count());
    println!("RESULT boot_to_init_ms {} ms", boot_ms);

    // --- Pilote console : un seul point d'entree, une page de registres, une IRQ.
    let ep_console = sys::endpoint_create().expect("endpoint console");
    let uart = sys::mmio_derive(slots::INIT_MMIO_ROOT, UART_A, 0x1000).expect("mmio uart");
    let uart_irq = sys::irq_get(slots::INIT_IRQ_CONTROL, UART_A_IRQ).expect("irq uart");
    let console_drv = process::spawn_elf(&Spawn {
        elf: fs.find("console").expect("console absent"),
        name: "console",
        grants: &[
            HandleGrant { handle: ep_console, slot: slots::DRV_SERVICE },
            HandleGrant { handle: uart, slot: slots::DRV_MMIO },
            HandleGrant { handle: uart_irq, slot: slots::DRV_IRQ },
        ],
        arg0: startup::IS_DRIVER,
        arg1: 0,
        prio: 4,
        cpu: None,
        strings: &Strings::new(&["console"], &[]),
    })
    .expect("lancement console");

    // init ne garde que le droit d'envoyer : le pilote est seul a pouvoir
    // recevoir, et s'il meurt, les clients l'apprennent (EPIPE).
    let console_send = sys::handle_dup(ep_console, SEND | GRANT | DUP, 0).expect("dup console");
    for h in [ep_console, uart, uart_irq, console_drv] {
        let _ = sys::handle_close(h);
    }
    console::set(console_send);
    println!("[init] pilote console lance (UART-A, IRQ {})", UART_A_IRQ);

    // --- Gestionnaire de processus et service de noms.
    let ep_pm = sys::endpoint_create().expect("endpoint pm");
    let pm_app = sys::handle_dup(ep_pm, SEND | GRANT | DUP, pm::BADGE_APP).expect("dup pm");
    let pm_srv = sys::handle_dup(ep_pm, SEND | GRANT | DUP, pm::BADGE_SERVICE).expect("dup pm");
    let mut st = State { fs, console: console_send, pm_app, pm_srv, services: Vec::new(), waiting: Vec::new() };

    for d in DRIVERS {
        if st.fs.find(d.name).is_none() {
            continue;
        }
        match start_driver(&st, d) {
            Ok(h) => {
                let _ = sys::handle_close(h);
            }
            Err(e) => println!("[init] {} : echec du lancement ({})", d.name, libnx::abi::err::name(e)),
        }
    }

    let shell = Strings::new(&["shell"], &[String::from("HOME=/"), String::from("PATH=/bin")]);
    match launch(&st, &shell, pm_app, &Stdio::CONSOLE) {
        Ok(h) => {
            let _ = sys::handle_close(h);
        }
        Err(e) => println!("[init] shell : erreur {}", libnx::abi::err::name(e)),
    }
    serve(&mut st, ep_pm)
}

/// Lance un pilote avec ses capacites materielles.
fn start_driver(st: &State, d: &Driver) -> Result<u32, i64> {
    let mut grants = Vec::new();
    let mut owned = Vec::new();
    grants.push(HandleGrant { handle: st.console, slot: slots::CONSOLE });
    grants.push(HandleGrant { handle: st.pm_srv, slot: slots::PM });
    let mut slot = slots::DEV_BASE;
    let r = (|| {
        for &(base, len) in d.mmio {
            let h = sys::mmio_derive(slots::INIT_MMIO_ROOT, base, len)?;
            owned.push(h);
            grants.push(HandleGrant { handle: h, slot });
            slot += 1;
        }
        for &irq in d.irqs {
            let h = sys::irq_get(slots::INIT_IRQ_CONTROL, irq)?;
            owned.push(h);
            grants.push(HandleGrant { handle: h, slot });
            slot += 1;
        }
        if d.sysctl {
            grants.push(HandleGrant { handle: slots::INIT_SYSCTL, slot });
        }
        process::spawn_elf(&Spawn {
            elf: st.fs.find(d.name).ok_or(-ENOENT)?,
            name: d.name,
            grants: &grants,
            arg0: startup::HAS_CONSOLE | startup::HAS_PM | startup::IS_DRIVER,
            arg1: 0,
            prio: d.prio,
            cpu: None,
            strings: &Strings::new(&[d.name], &[]),
        })
    })();
    // Les capacites derivees sont passees au pilote : init n'en garde rien.
    for h in owned {
        let _ = sys::handle_close(h);
    }
    r
}

/// Executable d'un programme : image de demarrage, sinon systeme de
/// fichiers (`/bin/NOM`, ou le chemin donne).
fn load(st: &State, name: &str) -> Result<Vec<u8>, i64> {
    if !name.contains('/') {
        if let Some(elf) = st.fs.find(name) {
            return Ok(elf.to_vec());
        }
    }
    let fs = st.services.iter().find(|s| s.name == "fs").ok_or(-ENOENT)?;
    libnx::fs::set_handle(fs.handle);
    let path = if name.contains('/') { String::from(name) } else { alloc::format!("/bin/{}", name) };
    libnx::fs::read(&path)
}

/// Entree, sortie et erreurs d'une application : tube recu du demandeur, ou
/// la console (et des drapeaux pour le programme : tube, rien).
struct Stdio {
    handles: [Option<u32>; 3],
    flags: u64,
}

impl Stdio {
    const CONSOLE: Stdio = Stdio { handles: [None; 3], flags: 0 };

    /// Tubes transmis avec une requete SPAWN (entree, sortie, erreurs).
    fn from_request(flags: u64, msg: &Msg) -> Stdio {
        let mut k = 0;
        let mut handles = [None; 3];
        for (i, bit) in [spawn::STDIN_PIPE, spawn::STDOUT_PIPE, spawn::STDERR_PIPE].into_iter().enumerate() {
            if flags & bit != 0 && k < msg.handles() {
                handles[i] = Some(ipc::received_handle(k));
                k += 1;
            }
        }
        let mut f = 0;
        for (i, bit) in [startup::STDIN_PIPE, startup::STDOUT_PIPE, startup::STDERR_PIPE].into_iter().enumerate() {
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

    /// Init ne garde pas les tubes : le programme a recu sa copie.
    fn close(&self) {
        for h in self.handles.iter().flatten() {
            let _ = sys::handle_close(*h);
        }
    }
}

/// Lance une application avec les capacites standard : console (ou tubes)
/// et gestionnaire (insigne application).
fn launch(st: &State, strings: &Strings, pm: u32, io: &Stdio) -> Result<u32, i64> {
    let path = strings.program();
    let elf = load(st, path)?;
    let name = path.rsplit('/').next().unwrap_or(path);
    let [stdin, stdout, stderr] = io.handles.map(|h| h.unwrap_or(st.console));
    // Drapeaux de lancement propres au noyau (arret pour debogueur) seulement.
    let mut strings = strings.clone();
    strings.flags &= spawn::SUSPENDED as u32;
    process::spawn_elf(&Spawn {
        elf: &elf,
        name,
        grants: &[
            HandleGrant { handle: stdout, slot: slots::CONSOLE },
            HandleGrant { handle: pm, slot: slots::PM },
            HandleGrant { handle: stdin, slot: slots::STDIN },
            HandleGrant { handle: stderr, slot: slots::STDERR },
        ],
        arg0: startup::HAS_CONSOLE | startup::HAS_PM | startup::HAS_STDIO | io.flags,
        arg1: 0,
        prio: 0,
        cpu: None,
        strings: &strings,
    })
}

/// Reponse portant un handle (dont init ferme ensuite sa copie).
fn reply_handle(h: u32) -> Msg {
    ipc::set_send_handle(0, h);
    Msg::new(0, 0, 1)
}

fn serve(st: &mut State, ep: u32) -> ! {
    let mut rx = ipc::recv(ep).expect("recv pm");
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
                        let r = launch(st, &s, st.pm_app, &io);
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
                let h = if rx.msg.handles() > 0 { ipc::received_handle(0) } else { 0 };
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
                    // Les recherches qui attendaient ce nom recoivent leur reponse.
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
                    st.services.push(Service { name, handle: h });
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
            _ => Msg::error(-ENOSYS),
        };
        if deferred {
            rx = ipc::recv(ep).expect("recv pm");
            continue;
        }
        if to_close != 0 {
            // Un handle est parti avec la reponse : on ferme notre copie tout
            // de suite, sans attendre la requete suivante.
            let _ = ipc::reply(&reply);
            let _ = sys::handle_close(to_close);
            rx = ipc::recv(ep).expect("recv pm");
            continue;
        }
        rx = match ipc::reply_recv(ep, &reply) {
            Ok(r) => r,
            Err(_) => ipc::recv(ep).expect("recv pm"),
        };
    }
}
