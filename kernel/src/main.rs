//! nxk: AArch64 Tegra X1 microkernel; EL1 keeps core mechanisms only.
//! Boot: console -> MMU -> GIC/timer -> memory -> SMP -> init.

#![no_std]
#![no_main]
#![deny(unsafe_op_in_unsafe_fn)]

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

/// Device range handed to `init` (the kernel keeps the GIC for itself).
/// From the PCIe aperture (configuration space, downstream windows and
/// the AFI, all below the APB blocks) up to where DRAM starts.
const MMIO_ROOT: (u64, u64) = (0x0100_0000, 0x7F00_0000);

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

/// Exception level nxk was started at: 3 with no secure monitor.
static mut BOOT_EL: usize = 3;

pub fn boot_el() -> usize {
    unsafe { BOOT_EL }
}

/// Core 0 entry point, called by `_start` once `.bss` is cleared.
#[no_mangle]
pub extern "C" fn kmain(core: usize, entry_el: usize, dtb: u64) -> ! {
    unsafe { BOOT_EL = entry_el };
    console::init();
    banner(core);

    let (mut dram_size, dram_src) = mmu::detect_dram();
    let mut holes = [(0u64, 0u64); mmu::MAX_HOLES + fdt::MAX_RESERVED];
    holes[..mmu::MAX_HOLES].copy_from_slice(&mmu::detect_holes());
    // Started by a monitor or loader: the device tree owns some regions.
    let dram = (mmu::DRAM_BASE as u64, (mmu::DRAM_BASE + dram_size) as u64);
    let fdt_info = if entry_el < 3 {
        fdt::parse(dtb, dram)
    } else {
        None
    };
    if let Some(f) = &fdt_info {
        for (k, &(b, s)) in f.reserved[..f.nreserved].iter().enumerate() {
            holes[mmu::MAX_HOLES + k] = mmu::round_hole(b, s);
        }
        // `/memory` may report less than the controller (loader-kept
        // memory above): the smaller size wins.
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
    // Fallback screen image: kept mapped (kernel writes to it) but never
    // handed out. Outside mapped DRAM or inside a hole: give up.
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
        "[rng] ChaCha20 seeded by {}",
        if se {
            "the SE hardware RNG and counter jitter"
        } else {
            "counter jitter only"
        }
    );

    let kend = sym(unsafe { &__kernel_end });
    let mut reserved = [(0u64, 0u64); mmu::MAX_HOLES + fdt::MAX_RESERVED + 1];
    reserved[..holes.len()].copy_from_slice(&holes);
    reserved[holes.len()] = fb.unwrap_or((0, 0));
    pmm::init(kend, mmu::dram_end(), &reserved);
    let (total, free, kpages) = pmm::stats();
    kprintln!(
        "[mem] {} MiB DRAM, {} MiB free (kernel and metadata: {} KiB)",
        (total * PAGE_SIZE) >> 20,
        (free * PAGE_SIZE) >> 20,
        (kpages * PAGE_SIZE) >> 10
    );
    if let mmu::DramSource::Probed { announced_mb } = dram_src {
        kprintln!(
            "[mem] MC_EMEM_CFG announces {} MiB: kept size after probing",
            announced_mb
        );
    }
    if let Some(f) = &fdt_info {
        kprintln!(
            "[boot] device tree at {:#x} ({} bytes), {} reserved region(s)",
            f.blob.0,
            f.blob.1,
            f.nreserved
        );
    }
    for &(b, e) in holes.iter().filter(|h| h.1 > h.0) {
        kprintln!(
            "[mem] monitor-reserved region: {:#x}..{:#x} ({} MiB)",
            b,
            e,
            (e - b) >> 20
        );
    }
    if let Some((b, e)) = fb {
        kprintln!(
            "[screen] messages mirrored to loader image: {:#x}..{:#x}",
            b,
            e
        );
    }
    let prefetch = mm::ahb_prefetch_off();
    if prefetch > 0 {
        kprintln!(
            "[mem] AHB memory prefetch turned off ({} were on)",
            prefetch
        );
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
                kprintln!(
                    "[boot] started in EL{} by a monitor: PSCI {}.{}",
                    entry_el,
                    maj,
                    min
                );
                smp::start_secondaries_psci(_start as *const () as usize)
            }
            None => {
                kprintln!("[boot] started in EL{} without PSCI: single core", entry_el);
                1
            }
        }
    };
    kprintln!("[smp] {}/{} cores online", online, percpu::NR_CPUS);

    start_init();
    timer::init_cpu();
    sched::exit_to_user()
}

/// Secondary core entry point.
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
    sched::adopt_pinned(core);
    sched::exit_to_user()
}

/// A core back from CPU_SUSPEND: its EL1/EL2 state and caches were lost,
/// the kernel's (threads, queues, deadlines) is all in memory.
#[no_mangle]
pub extern "C" fn kresume(core: usize) -> ! {
    unsafe { mmu::enable() };
    percpu::init(core, kstack_top(core), mmu::kernel_ttbr());
    arch::cpu_init();
    arch::exceptions::init();
    gic::init_cpu();
    bkl::acquire();
    timer::init_cpu();
    sched::resumed();
    sched::exit_to_user()
}

/// First process: read-only boot image plus root IRQ/MMIO/sysctl caps.
fn start_init() {
    let elf_image = bootfs::find("init").expect("boot image without 'init'");
    let pi = process::create(b"init", thread::NIL).expect("init: process");
    let src = elf::KernelSource(elf_image);
    let loaded = elf::load(&src, pi).expect("init: invalid ELF");
    let start = exec::prepare(pi, &src, &loaded, exec::Strings::Kernel(b"init\0"), 1, 0)
        .expect("init: stack");

    let img = bootfs::image();
    let base = img.as_ptr() as u64;
    let l1 = process::get(pi).l1;
    let mut off = 0;
    while off < img.len() as u64 {
        vm::map(
            l1,
            BOOTFS_BASE + off,
            base + off,
            vm::user_flags(prot::READ),
        )
        .expect("init: bootfs");
        off += PAGE_SIZE;
    }
    vm::publish();

    cap::insert_at(
        pi,
        slots::INIT_IRQ_CONTROL as usize,
        Cap::new(Kind::IrqControl, 0, 0, rights::ALL),
    )
    .expect("init: IRQ");
    let mut m = Cap::new(Kind::Mmio, (MMIO_ROOT.0 >> 12) as u32, 0, rights::ALL);
    m.extra = MMIO_ROOT.1;
    cap::insert_at(pi, slots::INIT_MMIO_ROOT as usize, m).expect("init: MMIO");
    cap::insert_at(
        pi,
        slots::INIT_SYSCTL as usize,
        Cap::new(Kind::SysControl, 0, 0, rights::ALL),
    )
    .expect("init: system control");
    cap::insert_at(
        pi,
        slots::INIT_KLOG as usize,
        Cap::new(Kind::Log, 0, 0, rights::GRANT | rights::DUP),
    )
    .expect("init: kernel log");

    let args = [startup::IS_INIT, img.len() as u64, start.block];
    process::create_thread(pi, loaded.entry, start.sp, args, 8, Some(0)).expect("init: thread");
    kprintln!(
        "[boot] boot image: {} programs, {} KiB; launching init",
        bootfs::count(),
        img.len() / 1024
    );
}

fn banner(core: usize) {
    kprintln!();
    kprintln!("================================================");
    kprintln!(
        " nxk v{}  -  Tegra X1 microkernel",
        env!("CARGO_PKG_VERSION")
    );
    kprintln!("================================================");
    kprintln!(
        "  boot core : {}   level : EL{} (entered in EL{})",
        core,
        arch::current_el(),
        boot_el()
    );
    kprintln!("  MIDR_EL1  : {:#010x}", sysreg_read!("midr_el1"));
    let (start, end) = (
        sym(unsafe { &__kernel_start }),
        sym(unsafe { &__kernel_end }),
    );
    kprintln!(
        "  kernel    : {:#x}..{:#x} ({} KiB)",
        start,
        end,
        (end - start) / 1024
    );
}

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    // Header without the formatting machinery: survives a broken `fmt`.
    use console::emergency_str as put_str;
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

    put_str("\n*** KERNEL PANIC cpu");
    put_num(arch::core_id() as u64);
    put_str(" ***\n");
    if let Some(loc) = info.location() {
        put_str("  at   : ");
        put_str(loc.file());
        put_str(":");
        put_num(loc.line() as u64);
        put_str("\n");
    }
    put_str("  what : ");
    console::_print_emergency(format_args!("{}\n", info.message()));
    arch::park()
}
