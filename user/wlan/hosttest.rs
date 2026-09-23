//! Host tests of wlan's pure modules (crypto, WPA2 handshakes), run on
//! the PC since the driver itself only builds for nxk:
//! `rustc --edition 2021 --test user/wlan/hosttest.rs -o T && ./T`

// The driver calls what the tests do not.
#![allow(dead_code)]

extern crate alloc;

#[path = "src/crypto.rs"]
mod crypto;
#[path = "src/wpa.rs"]
mod wpa;
