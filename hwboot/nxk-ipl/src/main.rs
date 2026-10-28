//! nxk-ipl: nxk's own RCM payload, run by the BPMP (ARM7TDMI) from IRAM.
//! It does what hekate does before its L4T launch (bdk `hw_init`, DRAM,
//! display, SD card), then starts nxk-bl31 on the CPU cluster. Ported
//! from hekate's bdk (GPL-2.0), checked against hekate's own run in the
//! emulator (`hwboot/oracle.sh`).

#![no_std]
#![no_main]

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
    ldr     sp, =0x4003FF00
    ldr     r0, =__bss_start
    ldr     r1, =__bss_end
    mov     r2, #0
2:  cmp     r0, r1
    strlo   r2, [r0], #4
    blo     2b
    ldr     r0, =ipl_main
    bx      r0
.pool
"#
);

#[no_mangle]
extern "C" fn ipl_main() -> ! {
    hw::init();
    uart::init_debug();
    println!("\n[nxk-ipl] hardware up, DRAM id {}", sdram::dram_id());
    // The panel and the boot screen first, as hekate's logo.
    display::init();
    display::window_a();
    display::splash(nxk_bootsplash::AT_IPL, true);
    display::backlight_pwm_init();
    display::brightness(100, 1000);
    match load() {
        Some((bl31, bl33, mtc_entries)) => {
            println!("[nxk-ipl] loaded bl31 {} bytes, bl33 {} bytes", bl31, bl33);
            display::splash(nxk_bootsplash::AT_LOADED, false);
            let mtc = train(mtc_entries);
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
    match minerva::init(entries) {
        Ok(m) => {
            println!("[nxk-ipl] DRAM trained, {} MHz", minerva::rate_mhz(&m));
            Some(m)
        }
        Err(e) => {
            println!("[nxk-ipl] DRAM training failed: {:?}", e);
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
                None => println!("[nxk-ipl] {} missing: the DRAM stays at its boot rate", name),
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
