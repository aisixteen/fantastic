//! The POSIX side of [`crate::sys`].

use std::ffi::{CStr, OsStr, OsString};
use std::os::unix::ffi::{OsStrExt, OsStringExt};

/// The C library's message for `errno` (XSI `strerror_r`).
pub(crate) fn strerror(errno: i32) -> String {
    let mut buf = [0 as libc::c_char; 512];
    // SAFETY: `buf` is a valid writable buffer of the length passed, and the
    // XSI `strerror_r` NUL-terminates what it writes on success.
    let rc = unsafe { libc::strerror_r(errno, buf.as_mut_ptr(), buf.len()) };
    if rc != 0 {
        return format!("Unknown error {errno}");
    }
    // SAFETY: on success `buf` holds a NUL-terminated C string.
    unsafe { CStr::from_ptr(buf.as_ptr()) }
        .to_string_lossy()
        .into_owned()
}

/// The `OSError` subclass CPython raises for `errno`; everything outside its
/// fixed set stays `OSError`.
pub(crate) fn os_error_class(errno: i32) -> &'static str {
    match errno {
        libc::EAGAIN | libc::EALREADY | libc::EINPROGRESS => "BlockingIOError",
        libc::EPIPE | libc::ESHUTDOWN => "BrokenPipeError",
        libc::ECHILD => "ChildProcessError",
        libc::ECONNABORTED => "ConnectionAbortedError",
        libc::ECONNREFUSED => "ConnectionRefusedError",
        libc::ECONNRESET => "ConnectionResetError",
        libc::EEXIST => "FileExistsError",
        libc::ENOENT => "FileNotFoundError",
        libc::EINTR => "InterruptedError",
        libc::EISDIR => "IsADirectoryError",
        libc::ENOTDIR => "NotADirectoryError",
        libc::EACCES | libc::EPERM => "PermissionError",
        libc::ESRCH => "ProcessLookupError",
        libc::ETIMEDOUT => "TimeoutError",
        _ => "OSError",
    }
}

/// The raw bytes of an OS string.
pub(crate) fn os_bytes(s: &OsStr) -> &[u8] {
    s.as_bytes()
}

/// An owned OS string's raw bytes.
pub(crate) fn os_into_bytes(s: OsString) -> Vec<u8> {
    s.into_vec()
}

/// An OS string from raw bytes.
pub(crate) fn os_from_bytes(b: Vec<u8>) -> OsString {
    OsString::from_vec(b)
}
