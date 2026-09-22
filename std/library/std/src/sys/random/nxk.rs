//! Random bytes: kernel ChaCha20 generator (Tegra security-engine seed).

pub fn fill_bytes(bytes: &mut [u8]) {
    crate::sys::pal::syscall::getrandom(bytes)
}
