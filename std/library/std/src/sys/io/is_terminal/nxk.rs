//! A std stream is a terminal if it goes to the console: neither pipe
//! nor null. Files never are.

use crate::sys::pal::abi::startup::{
    STDERR_NULL, STDERR_PIPE, STDIN_NULL, STDIN_PIPE, STDOUT_NULL, STDOUT_PIPE,
};
use crate::sys::pal::{startup_flags, svc};

pub fn is_terminal<T>(_: &T) -> bool {
    // Stdin, StdinLock, Stdout...: the type alone tells which stream it is
    // (`File` is never a terminal).
    let name = core::any::type_name::<T>();
    let (pipe, null) = if name.contains("Stdin") {
        (STDIN_PIPE, STDIN_NULL)
    } else if name.contains("Stdout") {
        (STDOUT_PIPE, STDOUT_NULL)
    } else if name.contains("Stderr") {
        (STDERR_PIPE, STDERR_NULL)
    } else {
        return false;
    };
    startup_flags() & (pipe | null) == 0 && svc::console() != 0
}
