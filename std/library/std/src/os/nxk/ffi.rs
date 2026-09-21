//! nxk-specific extensions to the primitives in the `std::ffi` module.
//!
//! Comme sous Unix, un `OsStr` est une suite d'octets quelconque (en pratique
//! de l'UTF-8 : noms FAT, arguments, environnement).

#![stable(feature = "rust1", since = "1.0.0")]

#[path = "../unix/ffi/os_str.rs"]
mod os_str;

#[stable(feature = "rust1", since = "1.0.0")]
pub use self::os_str::{OsStrExt, OsStringExt};
