//! CPython `OSError.__str__` and `UnicodeDecodeError.__str__`.

use std::path::Path;

use crate::repr::py_repr;

/// The C library's message for `errno` — what CPython's `OSError` carries.
#[must_use]
pub fn strerror(errno: i32) -> String {
    crate::sys::strerror(errno)
}

/// `str(OSError(errno, strerror(errno), path))`:
/// `[Errno N] <strerror>: <repr(path)>`, or without the `: …` tail when there
/// is no filename.
#[must_use]
pub fn os_error_str(errno: i32, path: Option<&str>) -> String {
    match path {
        Some(p) => format!("[Errno {errno}] {}: {}", strerror(errno), py_repr(p)),
        None => format!("[Errno {errno}] {}", strerror(errno)),
    }
}

/// The two-filename form (`os.rename`): `[Errno N] <strerror>: 'a' -> 'b'`.
#[must_use]
pub fn os_error_str2(errno: i32, path: &str, path2: &str) -> String {
    format!(
        "[Errno {errno}] {}: {} -> {}",
        strerror(errno),
        py_repr(path),
        py_repr(path2)
    )
}

/// An `std::io::Error` from a syscall on `path`, as python's `OSError` prints.
///
/// Every error `std::fs` returns for a syscall carries its errno; an error with
/// none did not come from the OS and keeps its own text.
#[must_use]
pub fn io_error_str(e: &std::io::Error, path: &Path) -> String {
    match e.raw_os_error() {
        Some(errno) => os_error_str(errno, Some(&path.to_string_lossy())),
        None => e.to_string(),
    }
}

/// `str(UnicodeDecodeError)` for the first invalid sequence in `bytes`:
/// `'utf-8' codec can't decode byte 0xff in position 0: invalid start byte`.
#[must_use]
pub fn utf8_decode_error_str(bytes: &[u8], err: &std::str::Utf8Error) -> String {
    let start = err.valid_up_to();
    let (len, reason) = match err.error_len() {
        None => (bytes.len() - start, "unexpected end of data"),
        Some(n) => {
            let first = bytes[start];
            let reason = if (0x80..=0xC1).contains(&first) || first >= 0xF5 {
                "invalid start byte"
            } else {
                "invalid continuation byte"
            };
            (n, reason)
        }
    };
    if len == 1 {
        format!(
            "'utf-8' codec can't decode byte 0x{:02x} in position {start}: {reason}",
            bytes[start]
        )
    } else {
        format!(
            "'utf-8' codec can't decode bytes in position {start}-{}: {reason}",
            start + len - 1
        )
    }
}

/// The `OSError` subclass CPython raises for `errno` — the class name that
/// prefixes a kernel's raised-reply text (`f"{type(e).__name__}: {e}"`).
///
/// CPython maps a fixed set of errnos to subclasses in `PyErr_SetFromErrno`;
/// everything else stays `OSError`.
#[must_use]
pub fn os_error_class(errno: i32) -> &'static str {
    crate::sys::os_error_class(errno)
}
