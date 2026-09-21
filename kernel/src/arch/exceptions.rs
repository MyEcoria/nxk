//! Vecteurs d'exception et entrees dans le noyau.
//!
//! Depuis EL0, `SP_EL1` pointe exactement sur la fin du `Frame` du thread
//! courant (positionne a la derniere sortie) : les registres utilisateur sont
//! donc ranges directement dans son TCB, sans copie. Le noyau bascule ensuite
//! sur la pile de son coeur, traite l'evenement et repart par `exit_to_user`,
//! qui peut reprendre un autre thread que celui qui est entre.
//!
//! Depuis EL1, seules deux choses arrivent : une interruption pendant que le
//! coeur est au repos, ou une faute du noyau lui-meme (fatale).

use core::arch::global_asm;

use crate::kern::{bkl, fault, irq, percpu, sched, syscall};
use crate::arch::{self, fpu};
use crate::{keprintln, sysreg_read, sysreg_write};

const KIND_SYNC: u64 = 0;
const KIND_IRQ: u64 = 1;
const KIND_FIQ: u64 = 2;
const KIND_SERROR: u64 = 3;
const KIND_AARCH32: u64 = 4;
const KIND_SP0: u64 = 9;

/// Cadre empile par les exceptions prises en EL1 (meme disposition que le
/// `Frame` des threads, champ `sp` inutilise).
#[repr(C)]
pub struct KFrame {
    pub x: [u64; 31],
    pub sp: u64,
    pub elr: u64,
    pub spsr: u64,
    pub _tpidr: u64,
    pub _pad: u64,
}

extern "C" {
    fn nxk_return_to_user(frame: *const crate::kern::thread::Frame) -> !;
}

#[no_mangle]
extern "C" fn nxk_el0_trap(_frame: u64, kind: u64) -> ! {
    // Chemin rapide : appels sans etat partage, traites sans verrou global ni
    // passage par l'ordonnanceur. Si un autre coeur demande une preemption
    // pendant ce temps, son IPI est en attente et se declenche des le retour
    // en EL0 : aucun reveil ne peut etre perdu.
    if kind == KIND_SYNC && sysreg_read!("esr_el1") >> 26 == 0x15 {
        let c = percpu::this();
        if !c.need_resched {
            let t = crate::kern::thread::get(c.current);
            let v = match t.frame.x[8] {
                nxk_abi::sys::NOP => Some(0),
                nxk_abi::sys::GETCPU => Some(t.home as u64),
                nxk_abi::sys::GETTID => Some(t.tid as u64),
                nxk_abi::sys::CLOCK => match t.frame.x[0] {
                    nxk_abi::clock::MONOTONIC => Some(crate::arch::timer::monotonic_ns()),
                    nxk_abi::clock::REALTIME => Some(crate::arch::timer::realtime_ns()),
                    _ => None,
                },
                _ => None,
            };
            if let Some(v) = v {
                t.frame.x[0] = v;
                c.stats.syscalls += 1;
                unsafe { nxk_return_to_user(&t.frame) }
            }
        }
    }

    bkl::acquire();
    let cur = percpu::this().current;
    match kind {
        KIND_SYNC => {
            let esr = sysreg_read!("esr_el1");
            match esr >> 26 {
                0x15 => syscall::dispatch(cur),
                0x07 => fpu::trap(cur),
                0x01 => sched::yield_current(cur), // WFI piege : traite comme yield
                _ => fault::user_fault(cur, esr, sysreg_read!("far_el1")),
            }
        }
        KIND_IRQ => irq::handle(),
        _ => fault::user_fault(cur, sysreg_read!("esr_el1"), 0),
    }
    sched::exit_to_user()
}

#[no_mangle]
extern "C" fn nxk_el1_trap(frame: &mut KFrame, kind: u64) {
    if kind == KIND_IRQ {
        // Coeur au repos : une interruption a pu rendre un thread pret.
        bkl::acquire();
        irq::handle();
        if cfg!(feature = "trace-sched") {
            crate::kprintln!("[s{}] irq au repos, travail {}", percpu::this().id, sched::has_work());
        }
        if sched::has_work() {
            sched::exit_to_user();
        }
        let c = percpu::this();
        if c.timer_dirty {
            crate::arch::timer::reprogram(c);
        }
        bkl::release();
        return;
    }
    kernel_fault(frame, kind);
}

fn kernel_fault(frame: &KFrame, kind: u64) -> ! {
    let esr = sysreg_read!("esr_el1");
    keprintln!("");
    keprintln!("*** FAUTE NOYAU (cpu{}) ***", arch::core_id());
    keprintln!("  type : {}  ESR {:#x} (EC {:#x})", kind, esr, esr >> 26);
    keprintln!("  ELR  : {:#018x}  FAR {:#018x}", frame.elr, sysreg_read!("far_el1"));
    keprintln!("  SPSR : {:#018x}", frame.spsr);
    for i in (0..30).step_by(2) {
        keprintln!("  x{:<2} {:#018x}  x{:<2} {:#018x}", i, frame.x[i], i + 1, frame.x[i + 1]);
    }
    keprintln!("  x30 {:#018x}", frame.x[30]);
    arch::park()
}

/// Installe la table de vecteurs sur le coeur courant.
pub fn init() {
    extern "C" {
        static nxk_vectors: u8;
    }
    let vbar = unsafe { &nxk_vectors as *const u8 as u64 };
    sysreg_write!("vbar_el1", vbar);
    arch::isb();
}

global_asm!(
    r#"
.section .text, "ax"

.equ FRAME_SIZE, 288

// Entree depuis EL0 : SP_EL1 = fin du Frame du thread courant.
.macro EL0_ENTRY kind
    sub     sp, sp, #FRAME_SIZE
    stp     x0, x1, [sp, #16 * 0]
    mov     x1, #\kind
    b       .Lel0_common
.endm

// Entree depuis EL1 : cadre empile sur la pile courante.
.macro EL1_ENTRY kind
    sub     sp, sp, #FRAME_SIZE
    stp     x0, x1, [sp, #16 * 0]
    mov     x1, #\kind
    b       .Lel1_common
.endm

.macro SAVE_X2_X29
    stp     x2,  x3,  [sp, #16 * 1]
    stp     x4,  x5,  [sp, #16 * 2]
    stp     x6,  x7,  [sp, #16 * 3]
    stp     x8,  x9,  [sp, #16 * 4]
    stp     x10, x11, [sp, #16 * 5]
    stp     x12, x13, [sp, #16 * 6]
    stp     x14, x15, [sp, #16 * 7]
    stp     x16, x17, [sp, #16 * 8]
    stp     x18, x19, [sp, #16 * 9]
    stp     x20, x21, [sp, #16 * 10]
    stp     x22, x23, [sp, #16 * 11]
    stp     x24, x25, [sp, #16 * 12]
    stp     x26, x27, [sp, #16 * 13]
    stp     x28, x29, [sp, #16 * 14]
.endm

.macro LOAD_X0_X29
    ldp     x0,  x1,  [sp, #16 * 0]
    ldp     x2,  x3,  [sp, #16 * 1]
    ldp     x4,  x5,  [sp, #16 * 2]
    ldp     x6,  x7,  [sp, #16 * 3]
    ldp     x8,  x9,  [sp, #16 * 4]
    ldp     x10, x11, [sp, #16 * 5]
    ldp     x12, x13, [sp, #16 * 6]
    ldp     x14, x15, [sp, #16 * 7]
    ldp     x16, x17, [sp, #16 * 8]
    ldp     x18, x19, [sp, #16 * 9]
    ldp     x20, x21, [sp, #16 * 10]
    ldp     x22, x23, [sp, #16 * 11]
    ldp     x24, x25, [sp, #16 * 12]
    ldp     x26, x27, [sp, #16 * 13]
    ldp     x28, x29, [sp, #16 * 14]
.endm

.Lel0_common:
    SAVE_X2_X29
    mrs     x9,  sp_el0
    stp     x30, x9,  [sp, #16 * 15]
    mrs     x10, elr_el1
    mrs     x11, spsr_el1
    stp     x10, x11, [sp, #16 * 16]
    mrs     x12, tpidr_el0
    str     x12, [sp, #16 * 17]
    mov     x0, sp
    // Bascule sur la pile noyau du coeur (PerCpu.kstack_top, offset 0).
    mrs     x9, tpidr_el1
    ldr     x10, [x9]
    mov     sp, x10
    mov     x29, #0
    bl      nxk_el0_trap
    brk     #1

.Lel1_common:
    SAVE_X2_X29
    mov     x9, sp
    add     x9, x9, #FRAME_SIZE
    stp     x30, x9,  [sp, #16 * 15]
    mrs     x10, elr_el1
    mrs     x11, spsr_el1
    stp     x10, x11, [sp, #16 * 16]
    mov     x0, sp
    bl      nxk_el1_trap
    ldp     x10, x11, [sp, #16 * 16]
    msr     elr_el1,  x10
    msr     spsr_el1, x11
    ldr     x30, [sp, #16 * 15]
    LOAD_X0_X29
    add     sp, sp, #FRAME_SIZE
    eret

// void nxk_return_to_user(const Frame *f) -> !
// Recharge les registres du thread choisi et laisse SP_EL1 sur la fin de son
// Frame : la prochaine entree depuis EL0 y rangera ses registres.
.globl nxk_return_to_user
.type nxk_return_to_user, @function
nxk_return_to_user:
    mov     sp, x0
    ldp     x10, x11, [sp, #16 * 16]
    msr     elr_el1,  x10
    msr     spsr_el1, x11
    ldr     x12, [sp, #16 * 17]
    msr     tpidr_el0, x12
    ldp     x30, x9,  [sp, #16 * 15]
    msr     sp_el0,   x9
    LOAD_X0_X29
    add     sp, sp, #FRAME_SIZE
    eret
.size nxk_return_to_user, . - nxk_return_to_user

// void nxk_enter_idle(u64 kstack_top) -> !
.globl nxk_enter_idle
.type nxk_enter_idle, @function
nxk_enter_idle:
    mov     sp, x0
    mov     x29, #0
    b       nxk_idle_loop
.size nxk_enter_idle, . - nxk_enter_idle

.align 11
.globl nxk_vectors
nxk_vectors:
    // EL courant, SP_EL0 : jamais utilise par le noyau
    .align 7
    EL1_ENTRY 9
    .align 7
    EL1_ENTRY 9
    .align 7
    EL1_ENTRY 9
    .align 7
    EL1_ENTRY 9
    // EL courant, SP_ELx : repos ou faute noyau
    .align 7
    EL1_ENTRY 0
    .align 7
    EL1_ENTRY 1
    .align 7
    EL1_ENTRY 2
    .align 7
    EL1_ENTRY 3
    // EL0 en AArch64
    .align 7
    EL0_ENTRY 0
    .align 7
    EL0_ENTRY 1
    .align 7
    EL0_ENTRY 2
    .align 7
    EL0_ENTRY 3
    // EL0 en AArch32 : non supporte
    .align 7
    EL0_ENTRY 4
    .align 7
    EL0_ENTRY 4
    .align 7
    EL0_ENTRY 4
    .align 7
    EL0_ENTRY 4
"#
);

const _: () = {
    let _ = KIND_FIQ;
    let _ = KIND_SERROR;
    let _ = KIND_AARCH32;
    let _ = KIND_SP0;
};
