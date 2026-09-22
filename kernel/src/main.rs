//! nxk - micro-noyau AArch64 pour Tegra X1 (Nintendo Switch).
//!
//! Le noyau ne fait que ce qui exige le privilege EL1 : espaces d'adressage,
//! threads, ordonnancement, IPC, capacites et routage des interruptions. Les
//! pilotes et les services tournent en EL0, comme des programmes ordinaires,
//! avec pour seuls droits ceux que leurs capacites leur donnent.
//!
//! Demarrage (coeur 0) : console -> MMU -> vecteurs -> GIC -> timer -> memoire
//! physique -> objets -> coeurs secondaires -> premier processus (`init`).

#![no_std]
#![no_main]
#![deny(unsafe_op_in_unsafe_fn)]
// Les pilotes exposent l'ensemble des registres et primitives du materiel meme
// si le noyau n'en utilise qu'une partie.
#![allow(dead_code)]

mod arch;
mod boot;
mod console;
mod drivers;
mod fdt;
mod kern;
mod mm;
mod mmio;
mod sync;

use core::panic::PanicInfo;

use nxk_abi::layout::{BOOTFS_BASE, PAGE_SIZE};
use nxk_abi::{prot, rights, slots, startup};

use arch::{gic, mmu, smp, timer};
use drivers::fbcon;
use kern::cap::{self, Cap, Kind};
use kern::{bkl, bootfs, elf, exec, ipc, notify, percpu, process, random, sched, thread};
use mm::{pmm, vm};

const KSTACK_SIZE: u64 = 0x4000;

/// Plage de peripheriques confiee a `init` (le noyau en retire le GIC).
const MMIO_ROOT: (u64, u64) = (0x5000_0000, 0x3000_0000);

extern "C" {
    static __stack_top: u8;
    static __kernel_start: u8;
    static __kernel_end: u8;
    fn _start();
}

fn sym(s: &u8) -> u64 {
    s as *const u8 as u64
}

fn kstack_top(core: usize) -> u64 {
    sym(unsafe { &__stack_top }) - core as u64 * KSTACK_SIZE
}

/// Niveau d'exception auquel nxk a ete lance : 3 sans moniteur securise.
static mut BOOT_EL: usize = 3;

pub fn boot_el() -> usize {
    unsafe { BOOT_EL }
}

/// Point d'entree du coeur 0, appele par `_start` une fois `.bss` efface.
#[no_mangle]
pub extern "C" fn kmain(core: usize, entry_el: usize, dtb: u64) -> ! {
    unsafe { BOOT_EL = entry_el };
    console::init();
    banner(core);

    let (mut dram_size, dram_src) = mmu::detect_dram();
    let mut holes = [(0u64, 0u64); mmu::MAX_HOLES + fdt::MAX_RESERVED];
    holes[..mmu::MAX_HOLES].copy_from_slice(&mmu::detect_holes());
    // Lance par un moniteur ou un chargeur : l'arbre de peripheriques dit
    // quelles zones lui appartiennent.
    let dram = (mmu::DRAM_BASE as u64, (mmu::DRAM_BASE + dram_size) as u64);
    let fdt_info = if entry_el < 3 { fdt::parse(dtb, dram) } else { None };
    if let Some(f) = &fdt_info {
        for (k, &(b, s)) in f.reserved[..f.nreserved].iter().enumerate() {
            holes[mmu::MAX_HOLES + k] = mmu::round_hole(b, s);
        }
        // `/memory` peut annoncer moins que le controleur (memoire gardee
        // par le chargeur au-dela) : la plus petite des deux tailles gagne.
        if let Some((base, size)) = f.memory {
            if base == mmu::DRAM_BASE as u64 && size >= 1 << 30 {
                dram_size = dram_size.min(size as usize & !(mmu::L2_SIZE - 1));
            }
        }
    }
    unsafe {
        mmu::build_tables(dram_size, &holes);
        mmu::enable();
    }
    console::enable_lock();
    // Image de l'ecran de secours : reste projetee (le noyau y ecrit) mais
    // n'est pas distribuee. Hors DRAM projetee ou dans un trou : abandon.
    let fb = fbcon::region().filter(|&(b, e)| {
        b >= dram.0 && e <= mmu::dram_end() && !holes.iter().any(|&(hb, he)| hb < e && he > b)
    });
    if fbcon::region().is_some() && fb.is_none() {
        fbcon::disable();
    }
    bkl::acquire();
    percpu::init(0, kstack_top(0), mmu::kernel_ttbr());
    arch::cpu_init();
    arch::exceptions::init();
    gic::init_dist();
    gic::init_cpu();
    timer::init_global();
    let se = random::init();
    kprintln!(
        "[rng] ChaCha20 amorce par {}",
        if se { "le generateur materiel du SE et la gigue du compteur" } else { "la gigue du compteur seule" }
    );

    let kend = sym(unsafe { &__kernel_end });
    let mut reserved = [(0u64, 0u64); mmu::MAX_HOLES + fdt::MAX_RESERVED + 1];
    reserved[..holes.len()].copy_from_slice(&holes);
    reserved[holes.len()] = fb.unwrap_or((0, 0));
    pmm::init(kend, mmu::dram_end(), &reserved);
    let (total, free, kpages) = pmm::stats();
    kprintln!(
        "[mem] {} MiB de DRAM, {} MiB libres (noyau et metadonnees : {} KiB)",
        total * PAGE_SIZE >> 20,
        free * PAGE_SIZE >> 20,
        kpages * PAGE_SIZE >> 10
    );
    if let mmu::DramSource::Probed { announced_mb } = dram_src {
        kprintln!(
            "[mem] MC_EMEM_CFG annonce {} MiB : taille retenue apres sondage",
            announced_mb
        );
    }
    if let Some(f) = &fdt_info {
        kprintln!(
            "[boot] arbre de peripheriques a {:#x} ({} octets), {} zone(s) reservee(s)",
            f.blob.0,
            f.blob.1,
            f.nreserved
        );
    }
    for &(b, e) in holes.iter().filter(|h| h.1 > h.0) {
        kprintln!("[mem] zone reservee par le moniteur : {:#x}..{:#x} ({} MiB)", b, e, (e - b) >> 20);
    }
    if let Some((b, e)) = fb {
        kprintln!("[ecran] messages recopies sur l'image du chargeur : {:#x}..{:#x}", b, e);
    }

    thread::init();
    process::init();
    ipc::init();
    notify::init();

    let online = if entry_el == 3 {
        smp::start_secondaries(_start as *const () as usize)
    } else {
        match arch::psci::probe() {
            Some((maj, min)) => {
                kprintln!("[boot] lance en EL{} par un moniteur : PSCI {}.{}", entry_el, maj, min);
                smp::start_secondaries_psci(_start as *const () as usize)
            }
            None => {
                kprintln!("[boot] lance en EL{} sans PSCI : un seul coeur", entry_el);
                1
            }
        }
    };
    kprintln!("[smp] {}/{} coeurs en ligne", online, percpu::NR_CPUS);

    start_init();
    timer::init_cpu();
    sched::exit_to_user()
}

/// Point d'entree des coeurs secondaires.
#[no_mangle]
pub extern "C" fn ksecondary(core: usize) -> ! {
    unsafe { mmu::enable() };
    percpu::init(core, kstack_top(core), mmu::kernel_ttbr());
    arch::cpu_init();
    arch::exceptions::init();
    gic::init_cpu();
    smp::mark_online();
    bkl::acquire();
    timer::init_cpu();
    sched::exit_to_user()
}

/// Cree le premier processus. Il recoit l'image de demarrage en lecture seule
/// et les capacites racines : controle des interruptions et plage des
/// peripheriques. C'est lui qui les distribuera aux pilotes.
fn start_init() {
    let elf_image = bootfs::find("init").expect("image de demarrage sans 'init'");
    let pi = process::create(b"init", thread::NIL).expect("init : processus");
    let src = elf::KernelSource(elf_image);
    let loaded = elf::load(&src, pi).expect("init : ELF invalide");
    let start = exec::prepare(pi, &src, &loaded, exec::Strings::Kernel(b"init\0"), 1, 0).expect("init : pile");

    let img = bootfs::image();
    let base = img.as_ptr() as u64;
    let l1 = process::get(pi).l1;
    let mut off = 0;
    while off < img.len() as u64 {
        vm::map(l1, BOOTFS_BASE + off, base + off, vm::user_flags(prot::READ)).expect("init : bootfs");
        off += PAGE_SIZE;
    }
    vm::publish();

    cap::insert_at(pi, slots::INIT_IRQ_CONTROL as usize, Cap::new(Kind::IrqControl, 0, 0, rights::ALL))
        .expect("init : IRQ");
    let mut m = Cap::new(Kind::Mmio, (MMIO_ROOT.0 >> 12) as u32, 0, rights::ALL);
    m.extra = MMIO_ROOT.1;
    cap::insert_at(pi, slots::INIT_MMIO_ROOT as usize, m).expect("init : MMIO");
    cap::insert_at(pi, slots::INIT_SYSCTL as usize, Cap::new(Kind::SysControl, 0, 0, rights::ALL))
        .expect("init : controle systeme");

    let args = [startup::IS_INIT, img.len() as u64, start.block];
    process::create_thread(pi, loaded.entry, start.sp, args, 8, Some(0)).expect("init : thread");
    kprintln!(
        "[boot] image de demarrage : {} programmes, {} KiB ; lancement de init",
        bootfs::count(),
        img.len() / 1024
    );
}

fn banner(core: usize) {
    kprintln!();
    kprintln!("================================================");
    kprintln!(" nxk v{}  -  micro-noyau Tegra X1", env!("CARGO_PKG_VERSION"));
    kprintln!("================================================");
    kprintln!("  coeur de demarrage : {}   niveau : EL{} (entree en EL{})", core, arch::current_el(), boot_el());
    kprintln!("  MIDR_EL1           : {:#010x}", sysreg_read!("midr_el1"));
    let (start, end) = (sym(unsafe { &__kernel_start }), sym(unsafe { &__kernel_end }));
    kprintln!("  image noyau        : {:#x}..{:#x} ({} KiB)", start, end, (end - start) / 1024);
}

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    // Ecriture brute : aucune machinerie `core::fmt` hors du message, pour
    // que la panique sorte meme si elle vient du formatage lui-meme. UART et
    // ecran de secours, sans verrou.
    use drivers::uart::{Uart, UART_A};
    struct Out(Uart);
    impl core::fmt::Write for Out {
        fn write_str(&mut self, s: &str) -> core::fmt::Result {
            self.0.write_str(s);
            fbcon::write_str(s);
            Ok(())
        }
    }
    let mut out = Out(Uart::new(UART_A));

    let put_str = |s: &str| {
        Uart::new(UART_A).write_str(s);
        fbcon::write_str(s);
    };
    let put_num = |mut n: u64| {
        let mut buf = [0u8; 20];
        let mut i = buf.len();
        loop {
            i -= 1;
            buf[i] = b'0' + (n % 10) as u8;
            n /= 10;
            if n == 0 {
                break;
            }
        }
        put_str(core::str::from_utf8(&buf[i..]).unwrap_or("?"));
    };

    put_str("\n*** PANIQUE NOYAU cpu");
    put_num(arch::core_id() as u64);
    put_str(" ***\n");
    if let Some(loc) = info.location() {
        put_str("  ou   : ");
        put_str(loc.file());
        put_str(":");
        put_num(loc.line() as u64);
        put_str("\n");
    }
    put_str("  quoi : ");
    let _ = core::fmt::write(&mut out, format_args!("{}", info.message()));
    put_str("\n");
    arch::park()
}
