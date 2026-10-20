//! PSCI 1.0 subset: VERSION, FEATURES, CPU_ON, CPU_OFF, AFFINITY_INFO, SYSTEM_RESET.
//! CPU_ON as exosphere: assert CPU/CORE reset, power the CPU partition, release reset; the core restarts at our `_start` (SB reset vector) and is sent to the caller's entry in EL2.
//! CPU_OFF as exosphere's `PowerOffCpuImpl`: the flow controller power-gates the core at its next WFI.

use core::ptr::{addr_of, addr_of_mut, read_volatile, write_volatile};

use crate::{rd, wr, BL33_DST, CAR_BASE, MON_BASE, PMC_BASE};

pub const NR_CPUS: usize = 4;

const VERSION: u32 = 0x8400_0000;
const FEATURES: u32 = 0x8400_000A;
const CPU_SUSPEND_64: u32 = 0xC400_0001;
const CPU_OFF: u32 = 0x8400_0002;
const CPU_ON_64: u32 = 0xC400_0003;
const AFFINITY_INFO_64: u32 = 0xC400_0004;
const SYSTEM_RESET: u32 = 0x8400_0009;
/// nxk SiP call (SMC64, SiP range): x1 = core; returns the `DIAG` words of
/// its last CPU_OFF and CPU_ON in x0-x3.
const NXK_DIAG: u32 = 0xC200_0001;
/// nxk SiP call: x1 = a core suspended by CPU_SUSPEND that no interrupt
/// woke: the flow controller powers it up (HALT_CPUn cleared).
const NXK_WAKE: u32 = 0xC200_0002;

const SUCCESS: i64 = 0;
const NOT_SUPPORTED: i64 = -1;
const DENIED: i64 = -3;
const INVALID_PARAMETERS: i64 = -2;
const ALREADY_ON: i64 = -4;
const ON_PENDING: i64 = -5;
const INTERNAL_FAILURE: i64 = -6;
const INVALID_ADDRESS: i64 = -9;

/// AFFINITY_INFO values, also our per-core state.
const ON: u32 = 0;
const OFF: u32 = 1;
const PENDING: u32 = 2;
/// Powered down by CPU_SUSPEND, back on its next interrupt (reported ON).
const SUSPENDED: u32 = 3;

const SB_AA64_RESET_LOW: usize = 0x6000_C230;
const SB_AA64_RESET_HIGH: usize = 0x6000_C234;
const SB_AA64_RST_AARCH64_MODE_EN: u32 = 1;

const PMC_CNTRL: usize = PMC_BASE;
const PMC_CNTRL_MAIN_RST: u32 = 1 << 4;
const PMC_PWRGATE_TOGGLE: usize = PMC_BASE + 0x30;
const PMC_REMOVE_CLAMPING: usize = PMC_BASE + 0x34;
const PMC_PWRGATE_STATUS: usize = PMC_BASE + 0x38;
const PWRGATE_TOGGLE_START: u32 = 1 << 8;
/// PMC partition per core: CE0, then CE1..CE3.
const CPU_PARTITION: [u32; NR_CPUS] = [14, 9, 10, 11];

const RST_CPUG_CMPLX_SET: usize = CAR_BASE + 0x450;

/// Flow controller (libvapours tegra_flow_ctlr.hpp): per-core offsets are
/// not a stride.
const FLOW_CTLR: usize = 0x6000_7000;
const FLOW_CPU_CSR: [usize; NR_CPUS] = [0x08, 0x18, 0x20, 0x28];
const FLOW_HALT_CPU: [usize; NR_CPUS] = [0x00, 0x14, 0x1C, 0x24];
const FLOW_CC4_CORE_CTRL: [usize; NR_CPUS] = [0x6C, 0x70, 0x74, 0x78];
/// CPUn_CSR: ENABLE, WAIT_WFI_BITMAP (11:8), ENABLE_EXT (13:12) = 0
/// (power-gate the CPU only), EVENT_FLAG and INTR_FLAG (write 1 to clear).
const CSR_ENABLE: u32 = 1 << 0;
const CSR_WFI_SHIFT: u32 = 8;
const CSR_FLAGS: u32 = 1 << 14 | 1 << 15;
/// HALT_CPUn_EVENTS.FLOW_MODE (31:29) = WAITEVENT, no wake source.
const HALT_WAITEVENT: u32 = 2 << 29;
/// Wake sources: GIC FIQ/IRQ (8, 9), legacy controller IRQ/FIQ (10, 11).
const HALT_ON_IRQ: u32 = 0xF << 8;
const RST_CPUG_CMPLX_CLR: usize = CAR_BASE + 0x454;

/// EL3 runs with caches off: volatile accesses are coherent between cores.
/// No atomics: exclusives need the MMU on. Each field has one writer at a time.
static mut STATE: [u32; NR_CPUS] = [OFF; NR_CPUS];
static mut ENTRY: [u64; NR_CPUS] = [0; NR_CPUS];
static mut CTX: [u64; NR_CPUS] = [0; NR_CPUS];
/// Per core: [0] CPU_ON outcome (step that brought it up, 0: none; bit 8:
/// partition on at entry; bit 9: still on after the gate wait), [1] at
/// CPU_OFF: PWRGATE_STATUS << 32 | CPUn_CSR, [2] at CPU_ON: the same,
/// [3] at CPU_ON: HALT_CPUn << 32 | PWRGATE_STATUS after the gate wait.
static mut DIAG: [[u64; 4]; NR_CPUS] = [[0; 4]; NR_CPUS];

fn diag_set(cpu: usize, i: usize, v: u64) {
    unsafe { write_volatile((addr_of_mut!(DIAG) as *mut u64).add(cpu * 4 + i), v) };
}

fn diag(cpu: usize, i: usize) -> u64 {
    unsafe { read_volatile((addr_of!(DIAG) as *const u64).add(cpu * 4 + i)) }
}

fn pwr_csr(cpu: usize) -> u64 {
    (rd(PMC_PWRGATE_STATUS) as u64) << 32 | rd(FLOW_CTLR + FLOW_CPU_CSR[cpu]) as u64
}

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
    fn _start_warm();
    /// Caches off, L1 flushed, SMPEN off, GIC CPU interface off, `wfi`
    /// (`resume` != 0: a `wfi` that returns goes to `_start_warm`).
    fn cpu_power_down(resume: u64) -> !;
}

/// Boot core: mark itself on, point the AArch64 reset vector at our
/// `_start_warm` (a secure write is allowed despite hekate's NS lock): from
/// now on a core powering up is never a cold boot.
pub fn init() {
    set_state(0, ON);
    let v = _start_warm as *const () as usize as u64;
    wr(SB_AA64_RESET_LOW, v as u32 | SB_AA64_RST_AARCH64_MODE_EN);
    wr(SB_AA64_RESET_HIGH, (v >> 32) as u32);
}

pub fn smc(regs: &mut [u64; 31]) {
    let r = match regs[0] as u32 {
        VERSION => 0x0001_0000,
        FEATURES => features(regs[1] as u32),
        CPU_ON_64 => cpu_on(regs[1], regs[2], regs[3]),
        CPU_OFF => cpu_off(),
        CPU_SUSPEND_64 => cpu_suspend(regs[1], regs[2], regs[3]),
        AFFINITY_INFO_64 => affinity_info(regs[1], regs[2]),
        SYSTEM_RESET => system_reset(),
        NXK_DIAG if (regs[1] as usize) < NR_CPUS => {
            let cpu = regs[1] as usize;
            for i in 1..4 {
                regs[i] = diag(cpu, i);
            }
            diag(cpu, 0) as i64
        }
        NXK_WAKE if (regs[1] as usize) < NR_CPUS => {
            let cpu = regs[1] as usize;
            if state(cpu) != SUSPENDED {
                INVALID_PARAMETERS
            } else {
                let step = power_up(cpu);
                diag_set(cpu, 0, step | 1 << 16);
                if step == 0 {
                    INTERNAL_FAILURE
                } else {
                    step as i64
                }
            }
        }
        _ => NOT_SUPPORTED,
    };
    regs[0] = r as u64;
}

fn features(fid: u32) -> i64 {
    match fid {
        VERSION | FEATURES | CPU_ON_64 | CPU_OFF | CPU_SUSPEND_64 | AFFINITY_INFO_64
        | SYSTEM_RESET => SUCCESS,
        _ => NOT_SUPPORTED,
    }
}

/// Callers serialize CPU_ON for a given core (nxk: boot core only). Returns
/// once the core reached `bl31_secondary`, trying in turn: the flow
/// controller's own power-up of a core it gated (HALT_CPUn cleared), a PMC
/// toggle, then a full PMC off/on cycle. `DIAG` keeps what happened.
fn cpu_on(mpidr: u64, entry: u64, ctx: u64) -> i64 {
    if mpidr >= NR_CPUS as u64 {
        return INVALID_PARAMETERS;
    }
    if !(BL33_DST as u64..MON_BASE as u64).contains(&entry) || !entry.is_multiple_of(4) {
        return INVALID_ADDRESS;
    }
    let cpu = mpidr as usize;
    match state(cpu) {
        ON | SUSPENDED => return ALREADY_ON,
        PENDING => return ON_PENDING,
        _ => {}
    }
    let part = CPU_PARTITION[cpu];
    let on_at_entry = partition_on(part);
    diag_set(cpu, 2, pwr_csr(cpu));
    // Just turned off: the reset must not hit its cache flush.
    wait_for(5000, || !partition_on(part));
    let on_after_wait = partition_on(part);
    diag_set(
        cpu,
        3,
        (rd(FLOW_CTLR + FLOW_HALT_CPU[cpu]) as u64) << 32 | rd(PMC_PWRGATE_STATUS) as u64,
    );
    unsafe {
        write_volatile((addr_of_mut!(ENTRY) as *mut u64).add(cpu), entry);
        write_volatile((addr_of_mut!(CTX) as *mut u64).add(cpu), ctx);
    }
    set_state(cpu, PENDING);

    let step = power_up(cpu);
    diag_set(
        cpu,
        0,
        step | (on_at_entry as u64) << 8 | (on_after_wait as u64) << 9,
    );
    let reset = 1 << cpu | 1 << (16 + cpu);
    if step == 0 {
        wr(RST_CPUG_CMPLX_SET, reset);
        set_state(cpu, OFF);
        return INTERNAL_FAILURE;
    }
    SUCCESS
}

/// Bring `cpu` up from a flow-controller gate or cold, until it reaches
/// `_start_warm` and sets itself ON: the flow controller's own power-up
/// (HALT_CPUn cleared), a PMC toggle, then a full PMC cycle. Returns the
/// step that worked, 0 if none.
fn power_up(cpu: usize) -> u64 {
    let part = CPU_PARTITION[cpu];
    let reset = 1 << cpu | 1 << (16 + cpu); // CPURESETn | CORERESETn
    let arrived = || wait_for(10_000, || state(cpu) == ON);
    wr(RST_CPUG_CMPLX_SET, reset);
    // 1: a core the flow controller gated comes back when un-halted.
    wr(FLOW_CTLR + FLOW_HALT_CPU[cpu], 0);
    let mut step = if wait_for(10_000, || partition_on(part)) {
        1
    } else {
        2
    };
    // 2: cold core (or the flow controller did not act): PMC toggle.
    let powered = step == 1 || power_on(part);
    if powered {
        wr(PMC_REMOVE_CLAMPING, 1 << part);
        wr(RST_CPUG_CMPLX_CLR, reset);
    }
    if !powered || !arrived() {
        // 3: full cycle, nothing armed in the flow controller.
        step = 3;
        wr(RST_CPUG_CMPLX_SET, reset);
        flow_reset(cpu);
        if partition_on(part) {
            toggle(part);
            wait_for(5000, || !partition_on(part));
        }
        let up = power_on(part) && {
            wr(PMC_REMOVE_CLAMPING, 1 << part);
            wr(RST_CPUG_CMPLX_CLR, reset);
            arrived()
        };
        if !up {
            step = 0;
        }
    }
    step
}

/// Start a PMC power toggle of `partition` (on <-> off).
fn toggle(partition: u32) -> bool {
    if !wait_for(5000, || rd(PMC_PWRGATE_TOGGLE) & PWRGATE_TOGGLE_START == 0) {
        return false;
    }
    wr(PMC_PWRGATE_TOGGLE, partition | PWRGATE_TOGGLE_START);
    true
}

fn partition_on(partition: u32) -> bool {
    rd(PMC_PWRGATE_STATUS) & (1 << partition) != 0
}

/// Ungate a CPU partition; 5 ms per step as exosphere.
fn power_on(partition: u32) -> bool {
    let on = || partition_on(partition);
    on() || toggle(partition) && wait_for(5000, on)
}

/// OFF only once the partition is gated (PSCI: not while still running).
fn affinity_info(mpidr: u64, level: u64) -> i64 {
    if level != 0 || mpidr >= NR_CPUS as u64 {
        return INVALID_PARAMETERS;
    }
    let cpu = mpidr as usize;
    match state(cpu) {
        OFF if partition_on(CPU_PARTITION[cpu]) => ON as i64,
        SUSPENDED => ON as i64,
        s => s as i64,
    }
}

fn this_cpu() -> usize {
    let mpidr: u64;
    unsafe { core::arch::asm!("mrs {}, mpidr_el1", out(reg) mpidr, options(nostack)) };
    (mpidr & 0xFF) as usize
}

/// Core 0 stays up (it alone serves SYSTEM_RESET and, later, the suspend).
fn cpu_off() -> i64 {
    let cpu = this_cpu();
    if cpu == 0 || cpu >= NR_CPUS {
        return DENIED;
    }
    diag_set(cpu, 1, pwr_csr(cpu));
    wr(
        FLOW_CTLR + FLOW_CPU_CSR[cpu],
        CSR_FLAGS | (1 << cpu) << CSR_WFI_SHIFT | CSR_ENABLE,
    );
    wr(FLOW_CTLR + FLOW_HALT_CPU[cpu], HALT_WAITEVENT);
    wr(FLOW_CTLR + FLOW_CC4_CORE_CTRL[cpu], 0);
    set_state(cpu, OFF);
    unsafe { cpu_power_down(0) }
}

/// Power-down state only (power_state bit 16): the core is gated at its
/// `wfi` and comes back on an interrupt, through `_start_warm`, at `entry`
/// in EL2 with x0 = `ctx`: its EL1/EL2 state is gone.
fn cpu_suspend(power_state: u64, entry: u64, ctx: u64) -> i64 {
    let cpu = this_cpu();
    if cpu >= NR_CPUS || power_state & (1 << 16) == 0 {
        return INVALID_PARAMETERS;
    }
    if !(BL33_DST as u64..MON_BASE as u64).contains(&entry) || !entry.is_multiple_of(4) {
        return INVALID_ADDRESS;
    }
    unsafe {
        write_volatile((addr_of_mut!(ENTRY) as *mut u64).add(cpu), entry);
        write_volatile((addr_of_mut!(CTX) as *mut u64).add(cpu), ctx);
    }
    wr(
        FLOW_CTLR + FLOW_CPU_CSR[cpu],
        CSR_FLAGS | (1 << cpu) << CSR_WFI_SHIFT | CSR_ENABLE,
    );
    wr(FLOW_CTLR + FLOW_HALT_CPU[cpu], HALT_WAITEVENT | HALT_ON_IRQ);
    wr(FLOW_CTLR + FLOW_CC4_CORE_CTRL[cpu], 0);
    set_state(cpu, SUSPENDED);
    unsafe { cpu_power_down(1) }
}

/// A core powering up after boot: CPU_ON's, or a suspended one waking.
#[no_mangle]
extern "C" fn bl31_warm(cpu: usize) -> ! {
    if cpu >= NR_CPUS {
        crate::park();
    }
    match state(cpu) {
        PENDING => bl31_secondary(cpu),
        SUSPENDED => {
            flow_reset(cpu);
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
        _ => crate::park(),
    }
}

/// A powered-up core: no power-gate armed for its next `wfi`.
fn flow_reset(cpu: usize) {
    wr(FLOW_CTLR + FLOW_CPU_CSR[cpu], CSR_FLAGS);
    wr(FLOW_CTLR + FLOW_HALT_CPU[cpu], 0);
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
    flow_reset(cpu);
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
