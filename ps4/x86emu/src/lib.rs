//! x86-64 decoder and interpreter for the nxk PS4 layer.
//!
//! The decoder is separate from the interpreter so that a JIT can later reuse it.
//! Correctness is checked on the PC by running the same code natively and comparing
//! registers, flags and memory (`tests/differential.rs`).
#![no_std]

pub mod cpu;
pub mod decode;
