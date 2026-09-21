//! FP/SIMD paresseux.
//!
//! Le noyau est compile sans FP : il ne touche jamais ces registres, donc rien
//! a sauvegarder sur un appel systeme ou une interruption. En EL0, l'acces est
//! piege tant que le thread n'est pas « proprietaire » du FP de son coeur. Au
//! premier usage, on sauve l'etat de l'ancien proprietaire et on charge le
//! sien. Un thread qui ne calcule qu'en entiers ne paie jamais ces 528 octets.
//! Un thread ne s'execute que sur son coeur d'attache, et c'est ce coeur qui
//! le fait migrer (apres `save_owner`) : un proprietaire par coeur suffit.

use core::arch::global_asm;

use crate::arch;
use crate::kern::percpu::{self, PerCpu};
use crate::kern::thread::{self, FpState, NIL};
use crate::sysreg_write;

const FPEN_TRAP_EL0: u64 = 0b01 << 20;
const FPEN_NONE: u64 = 0b11 << 20;

extern "C" {
    fn nxk_fp_save(s: *mut FpState);
    fn nxk_fp_restore(s: *const FpState);
}

pub fn init_cpu() {
    sysreg_write!("cpacr_el1", FPEN_TRAP_EL0);
    arch::isb();
    percpu::this().fp_enabled = false;
}

/// Avant de rendre la main a `i` : piege le FP sauf s'il en est proprietaire.
#[inline]
pub fn prepare(c: &mut PerCpu, i: u16) {
    let want = c.fp_owner == i;
    if want != c.fp_enabled {
        sysreg_write!("cpacr_el1", if want { FPEN_NONE } else { FPEN_TRAP_EL0 });
        arch::isb();
        c.fp_enabled = want;
    }
}

/// Sauve dans son TCB l'etat FP du proprietaire courant (avant sa migration) :
/// le coeur ne detient plus rien pour lui.
pub fn save_owner(c: &mut PerCpu) {
    if c.fp_owner != NIL {
        // FPEN=01 ne piege que l'EL0 : EL1 peut lire les registres FP.
        unsafe { nxk_fp_save(&mut thread::get(c.fp_owner).fp) };
        c.fp_owner = NIL;
    }
}

/// Premier usage du FP par `i` sur ce coeur.
pub fn trap(i: u16) {
    let c = percpu::this();
    if c.fp_owner != i {
        if c.fp_owner != NIL {
            unsafe { nxk_fp_save(&mut thread::get(c.fp_owner).fp) };
        }
        unsafe { nxk_fp_restore(&thread::get(i).fp) };
        c.fp_owner = i;
    }
}

global_asm!(
    r#"
.arch_extension fp
.arch_extension simd
.section .text, "ax"

.globl nxk_fp_save
.type nxk_fp_save, @function
nxk_fp_save:
    stp     q0,  q1,  [x0, #32 * 0]
    stp     q2,  q3,  [x0, #32 * 1]
    stp     q4,  q5,  [x0, #32 * 2]
    stp     q6,  q7,  [x0, #32 * 3]
    stp     q8,  q9,  [x0, #32 * 4]
    stp     q10, q11, [x0, #32 * 5]
    stp     q12, q13, [x0, #32 * 6]
    stp     q14, q15, [x0, #32 * 7]
    stp     q16, q17, [x0, #32 * 8]
    stp     q18, q19, [x0, #32 * 9]
    stp     q20, q21, [x0, #32 * 10]
    stp     q22, q23, [x0, #32 * 11]
    stp     q24, q25, [x0, #32 * 12]
    stp     q26, q27, [x0, #32 * 13]
    stp     q28, q29, [x0, #32 * 14]
    stp     q30, q31, [x0, #32 * 15]
    mrs     x1, fpcr
    mrs     x2, fpsr
    add     x3, x0, #512
    stp     x1, x2, [x3]
    ret
.size nxk_fp_save, . - nxk_fp_save

.globl nxk_fp_restore
.type nxk_fp_restore, @function
nxk_fp_restore:
    ldp     q0,  q1,  [x0, #32 * 0]
    ldp     q2,  q3,  [x0, #32 * 1]
    ldp     q4,  q5,  [x0, #32 * 2]
    ldp     q6,  q7,  [x0, #32 * 3]
    ldp     q8,  q9,  [x0, #32 * 4]
    ldp     q10, q11, [x0, #32 * 5]
    ldp     q12, q13, [x0, #32 * 6]
    ldp     q14, q15, [x0, #32 * 7]
    ldp     q16, q17, [x0, #32 * 8]
    ldp     q18, q19, [x0, #32 * 9]
    ldp     q20, q21, [x0, #32 * 10]
    ldp     q22, q23, [x0, #32 * 11]
    ldp     q24, q25, [x0, #32 * 12]
    ldp     q26, q27, [x0, #32 * 13]
    ldp     q28, q29, [x0, #32 * 14]
    ldp     q30, q31, [x0, #32 * 15]
    add     x3, x0, #512
    ldp     x1, x2, [x3]
    msr     fpcr, x1
    msr     fpsr, x2
    ret
.size nxk_fp_restore, . - nxk_fp_restore
"#
);
