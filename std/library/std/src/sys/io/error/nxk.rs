//! Error codes: Linux ones, returned negative by the kernel and services
//! (files, network). No per-thread errno: each call returns its error.

use crate::io::ErrorKind;
use crate::sys::pal::abi::err::*;

pub fn errno() -> i32 {
    0
}

pub fn is_interrupted(code: i32) -> bool {
    code as i64 == EINTR
}

pub fn decode_error_kind(code: i32) -> ErrorKind {
    use ErrorKind::*;
    match code as i64 {
        E2BIG => ArgumentListTooLong,
        EADDRINUSE => AddrInUse,
        EADDRNOTAVAIL => AddrNotAvailable,
        EBUSY => ResourceBusy,
        ECONNABORTED => ConnectionAborted,
        ECONNREFUSED => ConnectionRefused,
        ECONNRESET => ConnectionReset,
        EDEADLK => Deadlock,
        ENOTEMPTY => DirectoryNotEmpty,
        EEXIST => AlreadyExists,
        EFBIG => FileTooLarge,
        EHOSTUNREACH => HostUnreachable,
        EINTR => Interrupted,
        EINVAL => InvalidInput,
        EISDIR => IsADirectory,
        ELOOP => FilesystemLoop,
        ENOENT => NotFound,
        ENOMEM => OutOfMemory,
        ENOSPC => StorageFull,
        ENOSYS | EOPNOTSUPP | EAFNOSUPPORT | EPROTONOSUPPORT => Unsupported,
        EMLINK => TooManyLinks,
        ENAMETOOLONG => InvalidFilename,
        ENETDOWN => NetworkDown,
        ENETUNREACH => NetworkUnreachable,
        ENOTCONN => NotConnected,
        ENOTDIR => NotADirectory,
        EPIPE | ESHUTDOWN => BrokenPipe,
        EROFS => ReadOnlyFilesystem,
        ESPIPE => NotSeekable,
        ETIMEDOUT => TimedOut,
        EXDEV => CrossesDevices,
        EINPROGRESS => InProgress,
        EACCES | EPERM => PermissionDenied,
        EAGAIN => WouldBlock,
        EILSEQ => InvalidData,
        _ => Uncategorized,
    }
}

pub fn error_string(code: i32) -> String {
    let s = match code as i64 {
        EPERM => "operation not permitted",
        ENOENT => "no such file or directory",
        ESRCH => "no such process",
        EINTR => "interrupted call",
        EIO => "input/output error",
        ENXIO => "no such device or address",
        E2BIG => "argument list too long",
        ENOEXEC => "invalid executable format",
        EBADF => "bad handle",
        ECHILD => "no child process",
        EAGAIN => "resource temporarily unavailable",
        ENOMEM => "out of memory",
        EACCES => "permission denied",
        EFAULT => "bad address",
        EBUSY => "resource busy",
        EEXIST => "file already exists",
        EXDEV => "cross-device link",
        ENODEV => "no such device",
        ENOTDIR => "not a directory",
        EISDIR => "is a directory",
        EINVAL => "invalid argument",
        ENFILE | EMFILE => "too many open files",
        ENOTTY => "unsuitable device operation",
        EFBIG => "file too large",
        ENOSPC => "no space left on device",
        ESPIPE => "illegal seek",
        EROFS => "read-only filesystem",
        EMLINK => "too many links",
        EPIPE => "broken pipe",
        ERANGE => "result out of range",
        EDEADLK => "deadlock avoided",
        ENAMETOOLONG => "name too long",
        ENOSYS => "function not implemented",
        ENOTEMPTY => "directory not empty",
        ELOOP => "too many link levels",
        ENODATA => "no data",
        EOVERFLOW => "value too large",
        EILSEQ => "invalid byte sequence",
        ENOTSOCK => "not a socket",
        EDESTADDRREQ => "destination address required",
        EMSGSIZE => "message too long",
        EPROTOTYPE | ENOPROTOOPT | EPROTONOSUPPORT => "protocol unavailable",
        EOPNOTSUPP => "operation not supported",
        EAFNOSUPPORT => "address family not supported",
        EADDRINUSE => "address already in use",
        EADDRNOTAVAIL => "address unavailable",
        ENETDOWN => "network is down",
        ENETUNREACH => "network unreachable",
        ECONNABORTED => "connection aborted",
        ECONNRESET => "connection reset by peer",
        ENOBUFS => "no buffers available",
        EISCONN => "already connected",
        ENOTCONN => "not connected",
        ESHUTDOWN => "send side shut down",
        ETIMEDOUT => "timed out",
        ECONNREFUSED => "connection refused",
        EHOSTUNREACH => "host unreachable",
        EALREADY => "operation already in progress",
        EINPROGRESS => "operation in progress",
        _ => "unknown error",
    };
    s.to_string()
}
