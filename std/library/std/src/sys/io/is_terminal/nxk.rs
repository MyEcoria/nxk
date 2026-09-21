//! Un flux standard est un terminal s'il va a la console : ni tube, ni rien.
//! Les fichiers n'en sont jamais.

use crate::sys::pal::abi::startup::{
    STDERR_NULL, STDERR_PIPE, STDIN_NULL, STDIN_PIPE, STDOUT_NULL, STDOUT_PIPE,
};
use crate::sys::pal::{startup_flags, svc};

pub fn is_terminal<T>(_: &T) -> bool {
    // Stdin, StdinLock, Stdout... : le type suffit a savoir de quel flux il
    // s'agit (`File` n'est jamais un terminal).
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
