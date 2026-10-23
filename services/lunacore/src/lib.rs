//! Amazon Luna for nxk, the part with no I/O: what to send to Amazon's
//! identity service ("Panda") and Luna's proxy, how to read their replies,
//! the catalog an account can play and its covers. Ported from the desktop
//! client in `sources/luna` (docs/ANALYSIS.md there); `cargo test` runs on
//! the PC.

pub mod auth;
pub mod catalog;
pub mod config;
pub mod cover;
pub mod library;
pub mod pages;
pub mod proto;
pub mod proxy;
pub mod session;
pub mod stream;

use serde_json::Value;

/// A JSON number, or a number in a string (Amazon sends both).
pub fn as_u64(v: &Value) -> Option<u64> {
    v.as_u64()
        .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn numbers_come_as_numbers_or_strings() {
        assert_eq!(as_u64(&json!("30")), Some(30));
        assert_eq!(as_u64(&json!(30)), Some(30));
        assert_eq!(as_u64(&json!("x")), None);
    }
}
