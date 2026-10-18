//! nxk's shader toolchain: the IR games write programs in, its wire
//! format, the validator the GPU service runs on what it receives, and
//! a CPU reference machine. The Maxwell SASS lowering lives in
//! `lower` (used by the `gpu` service only).
#![cfg_attr(not(test), no_std)]

extern crate alloc;

pub mod builder;
pub mod fmath;
pub mod interp;
pub mod ir;
pub mod validate;
pub mod wire;

pub use builder::Builder;
pub use ir::*;
pub use validate::{validate, Info};

#[cfg(test)]
extern crate std;
#[cfg(test)]
mod tests;
