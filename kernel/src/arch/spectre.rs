//! Spectre variante 2 (CVE-2017-5715) sur Cortex-A57.
//!
//! Un processus pourrait entrainer le predicteur de branchements pour faire
//! executer speculativement, dans un autre processus, du code choisi. Le
//! Cortex-A57 oublie ses predictions sur `IC IALLU` a condition que le bit 0 de
//! CPUACTLR_EL1 soit pose : c'est fait en EL3 au demarrage (voir `boot.rs`),
//! puis le noyau invalide le predicteur a chaque changement de processus.
//! Les commutations entre threads d'un meme processus (et le chemin IPC
//! rapide entre un client et un serveur deja sur le coeur) n'en paient rien.

#[inline(always)]
pub fn on_process_switch() {
    if cfg!(feature = "spectre-v2") {
        unsafe { core::arch::asm!("ic iallu", "dsb nsh", "isb", options(nostack)) };
    }
}
