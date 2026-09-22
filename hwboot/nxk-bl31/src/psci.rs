//! PSCI 1.0 subset: VERSION, FEATURES, CPU_ON, AFFINITY_INFO, SYSTEM_RESET.
//! CPU_ON as exosphere: assert CPU/CORE reset, power the CPU partition, release reset; the core restarts at our `_start` (SB reset vector) and is sent to the caller's entry in EL2.

use core::ptr::{addr_of, addr_of_mut, read_volatile, write_volatile};

use crate::{rd, wr, BL33_DST, CAR_BASE, MON_BASE, PMC_BASE};

pub const NR_CPUS: usize = 4;

const VERSION: u32 = 0x8400_0000;
const FEATURES: u32 = 0x8400_000A;
const CPU_ON_64: u32 = 0xC400_0003;
const AFFINITY_INFO_64: u32 = 0xC400_0004;
const SYSTEM_RESET: u32 = 0x8400_0009;

const SUCCESS: i64 = 0;
const NOT_SUPPORTED: i64 = -1;
const INVALID_PARAMETERS: i64 = -2;
const ALREADY_ON: i64 = -4;
const ON_PENDING: i64 = -5;
const INTERNAL_FAILURE: i64 = -6;
const INVALID_ADDRESS: i64 = -9;

/// AFFINITY_INFO values, also our per-core state.
const ON: u32 = 0;
const OFF: u32 = 1;
const PENDING: u32 = 2;

const SB_AA64_RESET_LOW: usize = 0x6000_C230;
const SB_AA64_RESET_HIGH: usize = 0x6000_C234;
const SB_AA64_RST_AARCH64_MODE_EN: u32 = 1;

const PMC_CNTRL: usize = PMC_BASE;
const PMC_CNTRL_MAIN_RST: u32 = 1 << 4;
const PMC_PWRGATE_TOGGLE: usize = PMC_BASE + 0x30;
const PMC_PWRGATE_STATUS: usize = PMC_BASE + 0x38;
const PWRGATE_TOGGLE_START: u32 = 1 << 8;
/// PMC partition per core: CE0, then CE1..CE3.
const CPU_PARTITION: [u32; NR_CPUS] = [14, 9, 10, 11];

const RST_CPUG_CMPLX_SET: usize = CAR_BASE + 0x450;
const RST_CPUG_CMPLX_CLR: usize = CAR_BASE + 0x454;

/// EL3 runs with caches off: volatile accesses are coherent between cores.
/// No atomics: exclusives need the MMU on. Each field has one writer at a time.
static mut STATE: [u32; NR_CPUS] = [OFF; NR_CPUS];
static mut ENTRY: [u64; NR_CPUS] = [0; NR_CPUS];
static mut CTX: [u64; NR_CPUS] = [0; NR_CPUS];

fn state(cpu: usize) -> u32 {
    unsafe { read_volatile((addr_of!(STATE) as *const u32).add(cpu)) }
}

fn set_state(cpu: usize, s: u32) {
    unsafe {
        write_volatile((addr_of_mut!(STATE) as *mut u32).add(cpu), s);
        core::arch::asm!("dsb sy", options(nostack));
    }
}

fn ticks() -> u64 {
    let t: u64;
    unsafe { core::arch::asm!("isb", "mrs {}, cntpct_el0", out(reg) t, options(nostack)) };
    t
}

/// Poll `cond` for up to `us` microseconds (19.2 MHz counter).
fn wait_for(us: u64, cond: impl Fn() -> bool) -> bool {
    let end = ticks() + us * 96 / 5;
    while ticks() < end {
        if cond() {
            return true;
        }
    }
    cond()
}

extern "C" {
    fn _start();
}

/// Boot core: mark itself on, point the AArch64 reset vector at our `_start`
/// (hekate already does; a secure write is allowed despite its NS lock).
pub fn init() {
    set_state(0, ON);
    let v = _start as *const () as usize as u64;
    wr(SB_AA64_RESET_LOW, v as u32 | SB_AA64_RST_AARCH64_MODE_EN);
    wr(SB_AA64_RESET_HIGH, (v >> 32) as u32);
}

pub fn smc(regs: &mut [u64; 31]) {
    let r = match regs[0] as u32 {
        VERSION => 0x0001_0000,
        FEATURES => features(regs[1] as u32),
        CPU_ON_64 => cpu_on(regs[1], regs[2], regs[3]),
        AFFINITY_INFO_64 => affinity_info(regs[1], regs[2]),
        SYSTEM_RESET => system_reset(),
        _ => NOT_SUPPORTED,
    };
    regs[0] = r as u64;
}

fn features(fid: u32) -> i64 {
    match fid {
        VERSION | FEATURES | CPU_ON_64 | AFFINITY_INFO_64 | SYSTEM_RESET => SUCCESS,
        _ => NOT_SUPPORTED,
    }
}

/// Callers serialize CPU_ON for a given core (nxk: boot core only).
fn cpu_on(mpidr: u64, entry: u64, ctx: u64) -> i64 {
    if mpidr >= NR_CPUS as u64 {
        return INVALID_PARAMETERS;
    }
    if !(BL33_DST as u64..MON_BASE as u64).contains(&entry) || !entry.is_multiple_of(4) {
        return INVALID_ADDRESS;
    }
    let cpu = mpidr as usize;
    match state(cpu) {
        ON => return ALREADY_ON,
        PENDING => return ON_PENDING,
        _ => {}
    }
    unsafe {
        write_volatile((addr_of_mut!(ENTRY) as *mut u64).add(cpu), entry);
        write_volatile((addr_of_mut!(CTX) as *mut u64).add(cpu), ctx);
    }
    set_state(cpu, PENDING);

    let reset = 1 << cpu | 1 << (16 + cpu); // CPURESETn | CORERESETn
    wr(RST_CPUG_CMPLX_SET, reset);
    if !power_on(CPU_PARTITION[cpu]) {
        set_state(cpu, OFF);
        return INTERNAL_FAILURE;
    }
    wr(RST_CPUG_CMPLX_CLR, reset);
    SUCCESS
}

/// Ungate a CPU partition; 5 ms per step as exosphere.
fn power_on(partition: u32) -> bool {
    let on = || rd(PMC_PWRGATE_STATUS) & (1 << partition) != 0;
    if on() {
        return true;
    }
    if !wait_for(5000, || rd(PMC_PWRGATE_TOGGLE) & PWRGATE_TOGGLE_START == 0) {
        return false;
    }
    wr(PMC_PWRGATE_TOGGLE, partition | PWRGATE_TOGGLE_START);
    wait_for(5000, on)
}

fn affinity_info(mpidr: u64, level: u64) -> i64 {
    if level != 0 || mpidr >= NR_CPUS as u64 {
        return INVALID_PARAMETERS;
    }
    state(mpidr as usize) as i64
}

fn system_reset() -> i64 {
    wr(PMC_CNTRL, rd(PMC_CNTRL) | PMC_CNTRL_MAIN_RST);
    crate::park()
}

/// Core woken by `cpu_on`, from `_start` (per-core EL3 registers set).
#[no_mangle]
extern "C" fn bl31_secondary(cpu: usize) -> ! {
    if cpu >= NR_CPUS || state(cpu) != PENDING {
        crate::park();
    }
    crate::gic_secure_init_cpu();
    let (entry, ctx) = unsafe {
        (
            read_volatile((addr_of!(ENTRY) as *const u64).add(cpu)),
            read_volatile((addr_of!(CTX) as *const u64).add(cpu)),
        )
    };
    set_state(cpu, ON);
    unsafe { crate::drop_to_el2(entry, ctx) }
}
