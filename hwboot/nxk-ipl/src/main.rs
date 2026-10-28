//! nxk-ipl: nxk's own RCM payload, run by the BPMP (ARM7TDMI) from IRAM.
//! It does what hekate does before its L4T launch (bdk `hw_init`, DRAM,
//! display, SD card), then starts nxk-bl31 on the CPU cluster. Ported
//! from hekate's bdk (GPL-2.0), checked against hekate's own run in the
//! emulator (`hwboot/oracle.sh`).

#![no_std]
#![no_main]

mod battery;
mod ccplex;
mod clock;
mod di_tables;
mod display;
mod fat;
mod hdr;
mod hw;
mod i2c;
mod l4t;
mod minerva;
mod pmic;
mod reg;
mod report;
mod sd;
mod sdram;
mod sdram_seq;
mod sdram_tables;
mod soc;
mod timer;
mod uart;

use core::arch::global_asm;

global_asm!(
    r#"
.section .text._start, "ax"
.arm
.global _start
_start:
    // RCM leaves the payload at 0x40010000: move it to its link address
    // with a relocator copied out of the way (hekate start.S).
    adr     r0, _start
    ldr     r1, =__ipl_start
    cmp     r0, r1
    beq     _real_start
    adr     r2, _reloc
    ldr     r3, =0x4003FF00
    mov     r4, #(_real_start - _reloc)
1:  ldmia   r2!, {{r5}}
    stmia   r3!, {{r5}}
    subs    r4, r4, #4
    bne     1b
    ldr     r2, =__ipl_end
    sub     r2, r2, r1
    ldr     r3, =_real_start
    ldr     r4, =0x4003FF00
    bx      r4
_reloc:
    ldmia   r0!, {{r4-r7}}
    stmia   r1!, {{r4-r7}}
    subs    r2, r2, #0x10
    bne     _reloc
    bx      r3
_real_start:
    // SYS mode, IRQ masked, FIQ open for the DRAM training's watchdog.
    msr     cpsr_c, #0x9F
    ldr     sp, =0x4003FF00
    ldr     r0, =__bss_start
    ldr     r1, =__bss_end
    mov     r2, #0
2:  cmp     r0, r1
    strlo   r2, [r0], #4
    blo     2b
    ldr     r0, =ipl_main
    bx      r0

// WDT4's FIQ (hekate's watchdog_handle and BDK_RESTART_BL_ON_WDT): the
// watchdog off, its magic left above the stack, nxk-ipl started again.
// Banked r8-r12 only, no stack.
.global _fiq_handler
_fiq_handler:
    ldr     r8, =0x60005000
    mov     r9, #0
    str     r9, [r8, #0x80]
    ldr     r9, =0xC45A
    str     r9, [r8, #0x18C]
    mov     r9, #1
    str     r9, [r8, #0x188]
    mov     r9, #2
    str     r9, [r8, #0x188]
    mov     r9, #0x40000000
    str     r9, [r8, #0x8C]
    ldr     r8, =0x4003FF18
    ldr     r9, =0x544457
    str     r9, [r8]
    msr     cpsr_c, #0xDF
    b       _real_start
.pool
"#
);

/// hekate's `EXCP_TYPE_ADDR` and its watchdog value ("WDT"), set by
/// `_fiq_handler` above the stack and the cleared BSS.
const EXCP_TYPE_ADDR: u32 = 0x4003_FF18;
const EXCP_TYPE_WDT: u32 = 0x54_4457;

extern "C" {
    fn _fiq_handler();
}

/// `watchdog_fired`, read once and cleared, so that a later injection
/// trains again.
fn watchdog_fired() -> bool {
    let p = EXCP_TYPE_ADDR as *mut u32;
    let fired = unsafe { p.read_volatile() } == EXCP_TYPE_WDT;
    unsafe { p.write_volatile(0) };
    fired
}

#[no_mangle]
extern "C" fn ipl_main() -> ! {
    let wdt_fired = watchdog_fired();
    reg::wr(hdr::EXCP_VEC_BASE + hdr::EVP_COP_FIQ_VECTOR, _fiq_handler as usize as u32);
    hw::init();
    uart::init_debug();
    println!("\n[nxk-ipl] hardware up, DRAM id {}", sdram::dram_id());
    report::start();
    report::set(report::DRAM_ID, sdram::dram_id());
    if wdt_fired {
        report::flag(report::F_WATCHDOG);
    }
    // The battery before the panel, as hekate checks it.
    let lit = battery::check();
    pmic::rtc_prep_read();
    if !lit {
        display::init();
    }
    // The BPMP overclocked as hekate does it (back to 408 MHz at the launch).
    soc::bpmp_clk_rate_set(if soc::is_t210() { soc::BPMP_CLK_SUPER_BOOST } else { soc::BPMP_CLK_HYPER_BOOST });
    display::window_a();
    display::splash(nxk_bootsplash::AT_IPL, true);
    display::backlight_pwm_init();
    display::brightness(100, 1000);
    match load() {
        Some((bl31, bl33, mtc_entries)) => {
            println!("[nxk-ipl] loaded bl31 {} bytes, bl33 {} bytes", bl31, bl33);
            display::splash(nxk_bootsplash::AT_LOADED, false);
            let mtc = if wdt_fired {
                println!("[nxk-ipl] the watchdog fired in DRAM training: no training this time");
                None
            } else {
                train(mtc_entries)
            };
            l4t::launch(mtc)
        }
        None => println!("[nxk-ipl] ERROR: nxk/bl31.bin and nxk/bl33.bin not loaded"),
    }
    halt()
}

/// Where nxk-bl31 and nxk go (hekate's L4T layout): bl31 in TZDRAM,
/// bl33 where bl31 copies it from.
const TZDRAM_BASE: u32 = 0xFFF0_0000;
const BL33_LOAD_BASE: u32 = 0xAA00_0000;

/// Minerva's training (`minerva_init`), DRAM at 1600 MHz; None without a
/// table or on a failure (reported).
fn train(entries: usize) -> Option<nxk_emc::Mtc<'static>> {
    if entries == 0 {
        return None;
    }
    // hekate's watchdog (5 s): a hang restarts nxk-ipl, which then skips it.
    let start = timer::now_us();
    timer::watchdog_start(5_000_000 / 2);
    let r = minerva::init(entries);
    timer::watchdog_end();
    let ms = timer::now_us().wrapping_sub(start) / 1000;
    report::set(report::TRAIN_MS, ms);
    match r {
        Ok(m) => {
            println!("[nxk-ipl] DRAM trained in {} ms, {} MHz", ms, minerva::rate_mhz(&m));
            Some(m)
        }
        Err(e) => {
            println!("[nxk-ipl] DRAM training failed: {:?}", e);
            report::flag(report::F_TRAIN_FAILED);
            None
        }
    }
}

/// The card, its FAT32 volume, then the two stages of nxk and the DRAM
/// table.
fn load() -> Option<(u32, u32, usize)> {
    let (card, board) = match sd::open() {
        Ok(c) => c,
        Err(e) => {
            println!("[nxk-ipl] SD card: {}", nxk_sdcard::err_name(e));
            return None;
        }
    };
    println!("[nxk-ipl] SD card: {} MiB, {}-bit bus", card.blocks / 2048, card.bus_width());
    let r = (|| {
        let vol = fat::Volume::open(&card)?;
        let bl31 = vol.lookup(&card, "nxk/bl31.bin")?;
        let bl33 = vol.lookup(&card, "nxk/bl33.bin")?;
        let bl31 = vol.read(&card, &bl31, TZDRAM_BASE)?;
        let bl33 = vol.read(&card, &bl33, BL33_LOAD_BASE)?;
        // The DRAM training table (Erista): none means the boot rate.
        let mut entries = 0;
        if soc::is_t210() {
            let name = minerva::table_file(sdram::dram_id());
            match vol.lookup(&card, name).filter(|f| f.size <= minerva::BUF_BYTES) {
                Some(f) => entries = vol.read(&card, &f, minerva::BUF)? as usize / (nxk_emc::TABLE_WORDS * 4),
                None => {
                    println!("[nxk-ipl] {} missing: the DRAM stays at its boot rate", name);
                    report::flag(report::F_NO_TABLE);
                }
            }
        }
        Some((bl31, bl33, entries))
    })();
    sd::close(&card, &board);
    r
}

/// The BPMP stops for good (hekate `bpmp_halt`).
pub fn halt() -> ! {
    loop {
        reg::wr(reg::FLOW_CTLR + 0x4, timer::HALT_MODE_WAITEVENT | timer::HALT_JTAG);
    }
}

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    uart::puts("\n[nxk-ipl] panic\n");
    halt()
}
