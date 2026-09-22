//! Sequence de demarrage bas niveau.
//!
//! Lance directement (charge utile sans moniteur, ou l'emulateur), chaque
//! coeur CCPLEX demarre en **EL3**, MMU et caches desactives, avec `RVBAR` =
//! `_start`. On redescend EL3 -> EL2 -> EL1 : EL2 doit etre traverse pour
//! ouvrir l'acces au timer physique depuis EL1 (`CNTHCTL_EL2`) et annuler
//! `CNTVOFF_EL2`. Lance par un moniteur securise (ATF...), nxk arrive en EL2
//! ou en EL1 : les etapes deja faites par le moniteur sont sautees, et le
//! niveau d'entree est transmis a `kmain` (les coeurs secondaires seront
//! alors allumes par PSCI).
//!
//! Chaque coeur prend sa pile a `__stack_top - core_id * STACK_SIZE`. Seul le
//! coeur 0 remet `.bss` a zero, les secondaires ne sont liberes qu'apres.

use core::arch::global_asm;

global_asm!(
    r#"
.section .text.boot, "ax"
.globl _start
.type _start, @function

// SCR_EL3  = NS | RES1(4,5) | SMD | HCE | RW
.equ SCR_EL3_VAL,    0x5B1
// SPSR: DAIF masque + mode cible
.equ SPSR_EL2H,      0x3C9
.equ SPSR_EL1H,      0x3C5
// SCTLR_EL1 : bits RES1 ARMv8.0, MMU/caches off, acces non aligne autorise
.equ SCTLR_EL1_RES1, 0x30D00800
.equ STACK_SIZE,     0x4000

_start:
    mov     x21, x0                      // x21 = DTB eventuel (protocole arm64)
    mrs     x19, mpidr_el1
    and     x19, x19, #0xFF              // x19 = numero de coeur (Aff0)
    mrs     x20, CurrentEL
    lsr     x20, x20, #2                 // x20 = niveau d'entree (3 : sans moniteur)

    // ---- pile du coeur, posee tout de suite : le code securise en a besoin ----
    adrp    x0, __stack_top
    add     x0, x0, :lo12:__stack_top
    mov     x1, #STACK_SIZE
    msub    x0, x1, x19, x0              // sp = __stack_top - core_id * STACK_SIZE
    mov     sp, x0

    // ---- ou sommes-nous ? ----
    mrs     x0, CurrentEL
    lsr     x0, x0, #2
    cmp     x0, #3
    b.eq    .Lel3
    cmp     x0, #2
    b.eq    .Lel2
    b       .Lel1                        // deja en EL1

.Lel3:
    // Spectre v2 : sur Cortex-A57, IC IALLU n'invalide le predicteur de
    // branchements que si CPUACTLR_EL1[0] est pose, ce qu'EL3 seul peut faire.
    mrs     x0, S3_1_C15_C2_0            // CPUACTLR_EL1
    orr     x0, x0, #1
    msr     S3_1_C15_C2_0, x0
    // CPUECTLR_EL1.SMPEN : le TRM du Cortex-A57 l'exige avant d'activer
    // caches et MMU (maintenance diffusee, acces exclusifs des verrous).
    mrs     x0, S3_1_C15_C2_1            // CPUECTLR_EL1
    orr     x0, x0, #(1 << 6)
    msr     S3_1_C15_C2_1, x0
    // Frequence du compteur systeme : ecrivable en EL3 seulement, valeur
    // au reset INCONNUE sur le SoC (19,2 MHz = CLK_M, SYSCTR0_CNTFID0).
    movz    x0, #0xF800
    movk    x0, #0x0124, lsl #16
    msr     cntfrq_el0, x0
    isb

    // Le GIC demarre avec toutes ses lignes en Groupe 0 (monde securise) :
    // sans ce passage en Groupe 1, EL1 non-securise ne verra jamais une IRQ.
    mov     x0, x19
    bl      nxk_secure_gic_init

    mov     x0, #SCR_EL3_VAL
    msr     scr_el3, x0
    msr     cptr_el3, xzr                // pas de piege FP/SVE
    msr     mdcr_el3, xzr
    mov     x0, #SPSR_EL2H
    msr     spsr_el3, x0
    adr     x0, .Lel2
    msr     elr_el3, x0
    isb
    eret

.Lel2:
    // Registres EL2 a valeur de reset INCONNUE sur le SoC (l'emulateur les
    // met a zero) : EL2 sans MMU ni caches, petit-boutiste ; aucun piege des
    // registres de debogage et de performance (OSLAR_EL1, MDSCR_EL1... en
    // EL1), tous les compteurs PMU laisses a EL1 (HPMN = PMCR_EL0.N).
    movz    x0, #0x0830
    movk    x0, #0x30C5, lsl #16         // SCTLR_EL2 : bits RES1 seuls
    msr     sctlr_el2, x0
    mrs     x0, pmcr_el0
    ubfx    x0, x0, #11, #5
    msr     mdcr_el2, x0
    mov     x0, #1
    lsl     x0, x0, #31                  // HCR_EL2.RW : EL1 en AArch64
    msr     hcr_el2, x0
    mov     x0, #3                       // EL1PCTEN | EL1PCEN : timer physique visible en EL1
    msr     cnthctl_el2, x0
    msr     cntvoff_el2, xzr             // pas de decalage timer virtuel
    mov     x0, #0x33FF                  // CPTR_EL2 : aucun piege
    msr     cptr_el2, x0
    mrs     x0, midr_el1
    msr     vpidr_el2, x0
    mrs     x0, mpidr_el1
    msr     vmpidr_el2, x0
    movz    x0, #0x0800
    movk    x0, #0x30D0, lsl #16
    msr     sctlr_el1, x0                // etat connu avant le saut
    mov     x0, #SPSR_EL1H
    msr     spsr_el2, x0
    adr     x0, .Lel1
    msr     elr_el2, x0
    isb
    eret

.Lel1:
    mov     x0, #(3 << 20)               // CPACR_EL1.FPEN : FP/SIMD autorise (pour l'espace utilisateur)
    msr     cpacr_el1, x0
    isb

    // Chaque niveau d'exception a son propre SP en mode ELxh : la pile posee
    // en EL3 n'est pas celle d'EL1, il faut la reposer ici.
    adrp    x0, __stack_top
    add     x0, x0, :lo12:__stack_top
    mov     x1, #STACK_SIZE
    msub    x0, x1, x19, x0              // sp = __stack_top - core_id * STACK_SIZE
    mov     sp, x0

    cbnz    x19, .Lsecondary

    // ---- coeur 0 : .bss a zero ----
    adrp    x0, __bss_start
    add     x0, x0, :lo12:__bss_start
    adrp    x1, __bss_end
    add     x1, x1, :lo12:__bss_end
.Lbss_loop:
    cmp     x0, x1
    b.hs    .Lbss_done
    stp     xzr, xzr, [x0], #16
    b       .Lbss_loop
.Lbss_done:
    mov     x0, x19
    mov     x1, x20
    mov     x2, x21
    bl      kmain
    b       .Lpark

.Lsecondary:
    mov     x0, x19
    bl      ksecondary

.Lpark:
    msr     daifset, #0xF
.Lpark_loop:
    wfe
    b       .Lpark_loop

.size _start, . - _start
"#
);
