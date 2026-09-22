//! nxk-bl31 : moniteur EL3 minimal qui lance nxk depuis l'entree L4T de
//! hekate, sans ATF externe.
//!
//! Chaine materiellle (Erista) : RCM -> hekate -> ce moniteur (charge comme
//! `bl31.bin` en TZDRAM a 0xFFF00000) -> nxk (charge comme `bl33.bin` a
//! 0xAA000000, recopie a 0x80000000, entree en EL2, x0 = 0 = sans DTB).
//!
//! Le moniteur fait le minimum vital : reglages EL3 de chaque coeur que
//! l'emulateur n'exige pas mais le Cortex-A57 si (CNTFRQ_EL0, SMPEN,
//! registres EL2 a valeur de reset inconnue), horloge+reset d'UART-A puis
//! logs, recopies aussi sur l'ecran laisse allume par hekate (`fbcon`),
//! compteur systeme en marche, bascule du GIC en Groupe 1 (sans quoi EL1 non
//! securise ne voit aucune IRQ), declaration de sa region TZ au controleur
//! memoire (pour que nxk l'exclue de la DRAM libre), recopie de nxk et saut
//! en EL2. Les appels
//! `smc` (dont la sonde PSCI de nxk) recoivent NOT_SUPPORTED : nxk demarre
//! alors sur un seul coeur, sans PSCI. Pas de PSCI, pas de DTB, pas de
//! DVFS : chaque ajout serait du risque materiel inutile pour un premier
//! demarrage.
//!
//! Le meme binaire sert sous l'emulateur : QEMU depose le moniteur a
//! 0xFFF00000 et nxk.bin a 0xAA000000, vecteur de reset sur le moniteur
//! (voir `hwboot/test-bl31.sh`).

#![no_std]
#![no_main]

mod fbcon;

use core::arch::global_asm;
use core::ptr::{read_volatile, write_volatile};

/// Taille de nxk.bin, fournie a la compilation par `mkbl31.py`
/// (`NXK_BL33_SIZE`) : le moniteur ne fait pas confiance a son environnement.
fn bl33_size() -> u64 {
    match option_env!("NXK_BL33_SIZE") {
        Some(s) => parse_u64(s),
        None => 0,
    }
}

const BL33_SRC: usize = 0xAA00_0000; // ou hekate depose bl33.bin
const BL33_DST: usize = 0x8000_0000; // adresse de liaison de nxk
const MON_BASE: u32 = 0xFFF0_0000; // TZDRAM 1 Mo (Erista non-b01)

const UART_A: usize = 0x7000_6000;
const UART_CLK_HZ: u32 = 408_000_000;
const CAR_BASE: usize = 0x6000_6000;
const CAR_CLK_ENB_L_SET: usize = CAR_BASE + 0x320;
const CAR_RST_DEV_L_CLR: usize = CAR_BASE + 0x304;
const CAR_BIT_UARTA: u32 = 1 << 6;

const GICD: usize = 0x5004_1000;
const GICC: usize = 0x5004_2000;

const MC_SECURITY_CFG0: usize = 0x7001_9070;
const MC_SECURITY_CFG1: usize = 0x7001_9074;

/// Compteur systeme (source de CNTPCT_EL0) : EN | HDBG, comme exosphere.
const SYSCTR0_CNTCR: usize = 0x700F_0000;

const PMC_SCRATCH110: usize = 0x7000_E400 + 0xB10;
const IRAM_MAGIC: u32 = 0x4D41_5249; // "IRAM" : parametres hekate presents

const fn parse_u64(s: &str) -> u64 {
    let b = s.as_bytes();
    let mut i = 0;
    let mut v = 0u64;
    while i < b.len() {
        let d = match b[i] {
            c @ b'0'..=b'9' => c - b'0',
            b'_' => {
                i += 1;
                continue;
            }
            _ => panic!("NXK_BL33_SIZE invalide"),
        };
        v = v * 10 + d as u64;
        i += 1;
    }
    v
}

fn wr(a: usize, v: u32) {
    unsafe { write_volatile(a as *mut u32, v) }
}
fn rd(a: usize) -> u32 {
    unsafe { read_volatile(a as *const u32) }
}

/// UART-A a 115200 8N1. Ouvre d'abord horloge+reset via le CAR : sans cela
/// LSR ne bouge jamais et le premier `putc` attendrait pour toujours (c'est
/// aussi ce que fait le pilote UART de nxk).
fn uart_init() {
    wr(CAR_CLK_ENB_L_SET, CAR_BIT_UARTA);
    wr(CAR_RST_DEV_L_CLR, CAR_BIT_UARTA);
    // Indices 16550 espaces de 4 octets (registres * 4).
    let r = |i: usize| UART_A + i * 4;
    wr(r(1), 0);
    wr(r(3), 1 << 7); // DLAB
    let div = (UART_CLK_HZ + 115200 * 8) / (115200 * 16);
    wr(r(0), div & 0xFF);
    wr(r(1), (div >> 8) & 0xFF);
    wr(r(3), 0b11); // 8N1
    wr(r(2), (1 << 0) | (1 << 1) | (1 << 2)); // FIFO on + raz
}

fn putc(c: u8) {
    while rd(UART_A + 5 * 4) & (1 << 5) == 0 {}
    wr(UART_A, c as u32);
}
fn puts(s: &str) {
    for b in s.bytes() {
        if b == b'\n' {
            putc(b'\r');
        }
        putc(b);
    }
    fbcon::write_str(s);
}
fn puthex(v: u64) {
    let mut buf = *b"0x0000000000000000";
    for i in 0..16 {
        let d = ((v >> ((15 - i) * 4)) & 0xF) as u8;
        buf[2 + i] = if d < 10 { b'0' + d } else { b'a' + d - 10 };
    }
    puts(core::str::from_utf8(&buf).unwrap_or("0x?"));
}

/// Toutes les lignes en Groupe 1 : EL1 non securise recoit ses IRQ.
/// Secure-only (IGROUPR) : ne passe qu'en EL3, d'ou le moniteur.
fn gic_secure_init() {
    wr(GICD, 0);
    let lines = ((rd(GICD + 4) & 0x1F) + 1) * 32;
    for i in (0..lines).step_by(32) {
        wr(GICD + 0x80 + (i as usize / 32) * 4, 0xFFFF_FFFF);
    }
    wr(GICD, 0b11); // EnableGrp0 | EnableGrp1
    wr(GICD + 0x80, 0xFFFF_FFFF); // SGI/PPI du coeur 0
    wr(GICC, (1 << 0) | (1 << 1) | (1 << 3));
    wr(GICC + 4, 0xFF);
}

/// Recopie `size` octets puis invalide le cache d'instructions : on vient
/// d'ecrire du code qu'on va executer, MMU coupee.
fn relocate(src: usize, dst: usize, size: u64) {
    unsafe {
        core::ptr::copy_nonoverlapping(src as *const u8, dst as *mut u8, size as usize);
        core::arch::asm!("dsb sy", "ic ialluis", "dsb ish", "isb", options(nostack));
    }
}

/// Entree du coeur 0. Les secondaires sont parques en assembleur.
#[no_mangle]
extern "C" fn bl31_main() -> ! {
    uart_init();
    fbcon::init();
    puts("\n[nxk-bl31] moniteur EL3 (entree L4T hekate)\n");

    // Sans effet s'il tourne deja ; arrete, CNTPCT_EL0 resterait fige et
    // toute attente bornee de nxk durerait toujours.
    wr(SYSCTR0_CNTCR, rd(SYSCTR0_CNTCR) | 0b11);
    puts("[nxk-bl31] compteur systeme en marche, CNTFRQ = 19,2 MHz\n");

    gic_secure_init();
    puts("[nxk-bl31] GIC : tout en Groupe 1\n");

    // Declare notre Mo comme zone securisee : nxk lit ces registres
    // (`detect_holes`) et l'exclut de la DRAM libre. Meme paire que l4tboot.
    wr(MC_SECURITY_CFG0, MON_BASE);
    wr(MC_SECURITY_CFG1, 1);
    puts("[nxk-bl31] carveout TZ : 0xfff00000 (1 Mo)\n");

    if rd(PMC_SCRATCH110) == IRAM_MAGIC {
        puts("[nxk-bl31] parametres hekate presents (IRAM)\n");
    } else {
        puts("[nxk-bl31] pas de magie IRAM : lancement de test (emulateur)\n");
    }

    let size = bl33_size();
    if size == 0 || size > 8 << 20 {
        puts("[nxk-bl31] ERREUR : taille bl33 invalide (NXK_BL33_SIZE)\n");
        loop {
            unsafe { core::arch::asm!("wfe") };
        }
    }
    puts("[nxk-bl31] recopie bl33 : ");
    puthex(BL33_SRC as u64);
    puts(" -> ");
    puthex(BL33_DST as u64);
    puts(" (");
    puthex(size);
    puts(" octets)\n");
    relocate(BL33_SRC, BL33_DST, size);
    puts("[nxk-bl31] saut en EL2, x0 = 0 (sans DTB)\n");
    fbcon::handoff();

    unsafe { drop_to_bl33() }
}

extern "C" {
    fn drop_to_bl33() -> !;
}

/// `smc` venu d'EL2/EL1 : tout est refuse (pas de PSCI v1). nxk interprete
/// PSCI_VERSION = NOT_SUPPORTED comme "pas de moniteur PSCI" et demarre
/// sur un seul coeur. Tout autre piege EL3 est mortel (avec message).
#[no_mangle]
extern "C" fn smc_handler(regs: &mut [u64; 31]) {
    regs[0] = -1i64 as u64; // NOT_SUPPORTED
}

/// Acces piege ou exception inattendue : on l'affiche puis on parque.
#[no_mangle]
extern "C" fn el3_unexpected(regs: &mut [u64; 31]) {
    let (esr, elr): (u64, u64);
    unsafe {
        core::arch::asm!("mrs {}, esr_el3", out(reg) esr);
        core::arch::asm!("mrs {}, elr_el3", out(reg) elr);
    }
    puts("\n[nxk-bl31] piege EL3 inattendu, ESR=");
    puthex(esr);
    puts(" ELR=");
    puthex(elr);
    puts("\n");
    let _ = regs;
    loop {
        unsafe { core::arch::asm!("wfe") };
    }
}

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    puts("\n[nxk-bl31] panique\n");
    loop {
        unsafe { core::arch::asm!("wfe") };
    }
}

global_asm!(
    r#"
.section .text.boot, "ax"
.globl _start
_start:
    // Reglages EL3 de chaque coeur, ceux d'ATF et d'exosphere sur ce SoC
    // (valeurs de reset INCONNUES sur le silicium, sensees sous QEMU) :
    // - CNTFRQ_EL0 = 19,2 MHz (CLK_M), ecrivable en EL3 seulement ;
    // - CPUECTLR_EL1.SMPEN, que le TRM du Cortex-A57 exige avant d'activer
    //   caches et MMU (maintenance diffusee, acces exclusifs des verrous) ;
    // - CPUACTLR_EL1[0] : IC IALLU invalide aussi le predicteur de
    //   branchements (mitigation Spectre v2 de nxk, comme son boot.rs).
    movz    x0, #0xF800
    movk    x0, #0x0124, lsl #16
    msr     cntfrq_el0, x0
    mrs     x0, S3_1_C15_C2_1           // CPUECTLR_EL1
    orr     x0, x0, #(1 << 6)
    msr     S3_1_C15_C2_1, x0
    mrs     x0, S3_1_C15_C2_0           // CPUACTLR_EL1
    orr     x0, x0, #1
    msr     S3_1_C15_C2_0, x0
    isb
    mrs     x19, mpidr_el1
    and     x19, x19, #0xFF
    adrp    x0, __stack_top
    add     x0, x0, :lo12:__stack_top
    mov     x1, #0x4000
    msub    x0, x1, x19, x0
    mov     sp, x0
    adrp    x0, el3_vectors
    add     x0, x0, :lo12:el3_vectors
    msr     vbar_el3, x0
    isb
    cbnz    x19, .Lpark                 // secondaires : parques
    adrp    x0, __bss_start
    add     x0, x0, :lo12:__bss_start
    adrp    x1, __bss_end
    add     x1, x1, :lo12:__bss_end
.Lbss:
    cmp     x0, x1
    b.hs    .Lbss_done
    str     xzr, [x0], #8
    b       .Lbss
.Lbss_done:
    bl      bl31_main
.Lpark:
    msr     daifset, #0xF
.Lpark_loop:
    wfe
    b       .Lpark_loop

// void drop_to_bl33() -> ! : EL3 -> EL2, ELR = 0x80000000, x0 = 0.
.globl drop_to_bl33
drop_to_bl33:
    // SCR_EL3 = NS | RES1 | SMD=0 | HCE | RW : les `smc` piegent en EL3
    // (notre gestionnaire les refuse proprement).
    mov     x0, #0x531
    msr     scr_el3, x0
    msr     cptr_el3, xzr
    msr     mdcr_el3, xzr
    msr     cntvoff_el2, xzr
    // EL2 dans un etat connu (comme ATF) : SCTLR_EL2 reduit a ses bits
    // RES1 (MMU et caches coupes, petit-boutiste), aucun piege de debogage
    // ni de PMU vers EL2 (HPMN = PMCR_EL0.N), pas de piege AArch32, pas
    // de traduction de niveau 2, timer d'hyperviseur arrete.
    movz    x0, #0x0830
    movk    x0, #0x30C5, lsl #16
    msr     sctlr_el2, x0
    mrs     x0, pmcr_el0
    ubfx    x0, x0, #11, #5
    msr     mdcr_el2, x0
    msr     hstr_el2, xzr
    msr     vttbr_el2, xzr
    msr     cnthp_ctl_el2, xzr
    mov     x0, #1
    lsl     x0, x0, #31                // HCR_EL2.RW : EL1 en AArch64
    msr     hcr_el2, x0
    mov     x0, #3                     // timer physique visible en EL1
    msr     cnthctl_el2, x0
    mrs     x0, midr_el1
    msr     vpidr_el2, x0
    mrs     x0, mpidr_el1
    msr     vmpidr_el2, x0
    mov     x0, #0x33FF
    msr     cptr_el2, x0
    movz    x0, #0x0800
    movk    x0, #0x30D0, lsl #16
    msr     sctlr_el1, x0
    mov     x0, #0x3C9                 // SPSR : EL2h, tout masque
    msr     spsr_el3, x0
    movz    x0, #0x8000, lsl #16       // ELR = 0x80000000
    msr     elr_el3, x0
    mov     x0, xzr                    // x0 = 0 : pas de DTB
    mov     x1, xzr
    mov     x2, xzr
    mov     x3, xzr
    isb
    eret

.macro SAVE
    sub     sp, sp, #256
    stp     x0, x1, [sp, #0]
    stp     x2, x3, [sp, #16]
    stp     x4, x5, [sp, #32]
    stp     x6, x7, [sp, #48]
    stp     x8, x9, [sp, #64]
    stp     x10, x11, [sp, #80]
    stp     x12, x13, [sp, #96]
    stp     x14, x15, [sp, #112]
    stp     x16, x17, [sp, #128]
    stp     x18, x19, [sp, #144]
    stp     x20, x21, [sp, #160]
    stp     x22, x23, [sp, #176]
    stp     x24, x25, [sp, #192]
    stp     x26, x27, [sp, #208]
    stp     x28, x29, [sp, #224]
    str     x30, [sp, #240]
.endm
.macro RESTORE
    ldp     x0, x1, [sp, #0]
    ldp     x2, x3, [sp, #16]
    ldp     x4, x5, [sp, #32]
    ldp     x6, x7, [sp, #48]
    ldp     x8, x9, [sp, #64]
    ldp     x10, x11, [sp, #80]
    ldp     x12, x13, [sp, #96]
    ldp     x14, x15, [sp, #112]
    ldp     x16, x17, [sp, #128]
    ldp     x18, x19, [sp, #144]
    ldp     x20, x21, [sp, #160]
    ldp     x22, x23, [sp, #176]
    ldp     x24, x25, [sp, #192]
    ldp     x26, x27, [sp, #208]
    ldp     x28, x29, [sp, #224]
    ldr     x30, [sp, #240]
    add     sp, sp, #256
.endm

.Lsmc_entry:
    SAVE
    mrs     x9, esr_el3
    lsr     x9, x9, #26
    mov     x0, sp
    cmp     x9, #0x17                  // SMC depuis AArch64 ?
    b.ne    .Lsmc_bad
    bl      smc_handler
    RESTORE
    eret
.Lsmc_bad:
    bl      el3_unexpected             // ne revient pas
    RESTORE
    eret
.Lbad:
    SAVE
    mov     x0, sp
    bl      el3_unexpected             // ne revient pas
    RESTORE
    eret

.align 11
el3_vectors:
    .align 7
    b .Lbad
    .align 7
    b .Lbad
    .align 7
    b .Lbad
    .align 7
    b .Lbad
    .align 7
    b .Lbad
    .align 7
    b .Lbad
    .align 7
    b .Lbad
    .align 7
    b .Lbad
    // niveau inferieur AArch64 : sync (smc), irq, fiq, serror
    .align 7
    b .Lsmc_entry
    .align 7
    b .Lbad
    .align 7
    b .Lbad
    .align 7
    b .Lbad
    .align 7
    b .Lbad
    .align 7
    b .Lbad
    .align 7
    b .Lbad
    .align 7
    b .Lbad
"#
);
