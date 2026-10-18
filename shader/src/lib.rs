//! nxk's shader toolchain: the IR games write programs in, its wire
//! format, the validator the GPU service runs on what it receives, and
//! a CPU reference machine. The Maxwell SASS lowering lives in
//! `lower` (used by the `gpu` service only).
#![cfg_attr(not(test), no_std)]

extern crate alloc;

pub mod builder;
pub mod emit;
pub mod fmath;
pub mod fuzz;
pub mod interp;
pub mod ir;
pub mod lower;
pub mod regalloc;
pub mod rt;
pub mod sass;
pub mod stock;
pub mod validate;
pub mod wire;

pub use builder::Builder;
pub use ir::*;
pub use validate::{validate, Info};

#[cfg(test)]
extern crate std;
#[cfg(test)]
mod layout_tests;
#[cfg(test)]
pub(crate) mod lower_tests;
#[cfg(test)]
mod sassim;
#[cfg(test)]
mod tests;
