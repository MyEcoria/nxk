//! Octets aleatoires : generateur ChaCha20 du noyau (graine materielle du
//! moteur de securite du Tegra).

pub fn fill_bytes(bytes: &mut [u8]) {
    crate::sys::pal::syscall::getrandom(bytes)
}
