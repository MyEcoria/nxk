//! Coeur du micro-noyau : objets, ordonnancement, IPC, appels systeme.

pub mod bkl;
pub mod bootfs;
pub mod cap;
pub mod debug;
pub mod elf;
pub mod exec;
pub mod fault;
pub mod futex;
pub mod ipc;
pub mod irq;
pub mod notify;
pub mod objtab;
pub mod percpu;
pub mod process;
pub mod random;
pub mod sched;
pub mod shm;
pub mod syscall;
pub mod thread;
