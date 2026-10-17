//! The Windows side of [`crate::sys`].
//!
//! An OS error here is a Win32 code; python's `OSError` carries the errno
//! CPython maps it to (`PC/errmap.h` `winerror_to_errno`), with the errno
//! numbers python's `errno` module has on Windows (the C runtime's, and the
//! Winsock codes for the socket names). The text is the C runtime's message for
//! that errno — `[Errno 2] No such file or directory` reads the same on every OS.

use std::ffi::{CStr, OsString};
use std::path::{Component, Path, PathBuf, Prefix};

use crate::oserror::os_error_str;

// The C runtime's errno numbers (python's `errno` on Windows).
const EPERM: i32 = 1;
const ENOENT: i32 = 2;
const ESRCH: i32 = 3;
const EINTR: i32 = 4;
const ENOEXEC: i32 = 8;
const EBADF: i32 = 9;
const ECHILD: i32 = 10;
const EAGAIN: i32 = 11;
const ENOMEM: i32 = 12;
const EACCES: i32 = 13;
const EEXIST: i32 = 17;
const EXDEV: i32 = 18;
const ENOTDIR: i32 = 20;
const EISDIR: i32 = 21;
const EINVAL: i32 = 22;
const EMFILE: i32 = 24;
const ENOSPC: i32 = 28;
const EPIPE: i32 = 32;
const E2BIG: i32 = 7;
const ENOTEMPTY: i32 = 41;
const EILSEQ: i32 = 42;
// The socket errno names python's `errno` binds to their Winsock codes.
const EWOULDBLOCK: i32 = 10035;
const EINPROGRESS: i32 = 10036;
const EALREADY: i32 = 10037;
const ECONNABORTED: i32 = 10053;
const ECONNRESET: i32 = 10054;
const ESHUTDOWN: i32 = 10058;
const ETIMEDOUT: i32 = 10060;
const ECONNREFUSED: i32 = 10061;

/// The errno python's `OSError` carries for this OS error: the Win32 code
/// mapped by CPython's `winerror_to_errno`.
pub(crate) fn errno_of(e: &std::io::Error) -> Option<i32> {
    e.raw_os_error().map(winerror_to_errno)
}

/// CPython `PC/errmap.h` `winerror_to_errno`.
fn winerror_to_errno(code: i32) -> i32 {
    // A FACILITY_WIN32 HRESULT wraps a Win32 code.
    let code = if (code as u32) & 0xFFFF_0000 == 0x8007_0000 {
        code & 0xFFFF
    } else {
        code
    };
    if (10000..12000).contains(&code) {
        // Winsock: the codes ARE the errno, but for the six that shadow a C one.
        return match code {
            10004 | 10009 | 10013 | 10014 | 10022 | 10024 => code - 10000,
            _ => code,
        };
    }
    match code {
        2 | 3 | 15 | 18 | 53 | 67 | 161 | 206 => ENOENT,
        10 => E2BIG,
        11 | 188..=202 => ENOEXEC,
        6 | 114 | 130 => EBADF,
        128 | 129 => ECHILD,
        89 | 164 | 215 => EAGAIN,
        7 | 8 | 9 | 1816 => ENOMEM,
        5 | 16 | 19..=36 | 65 | 82 | 83 | 108 | 132 | 158 | 167 => EACCES,
        80 | 183 => EEXIST,
        17 => EXDEV,
        267 => ENOTDIR,
        4 => EMFILE,
        112 => ENOSPC,
        109 | 232 => EPIPE,
        145 => ENOTEMPTY,
        1113 => EILSEQ,
        _ => EINVAL,
    }
}

/// The C runtime's message for `errno`; a Winsock code's is the system's
/// message for it, as CPython prints it (trailing period dropped).
pub(crate) fn strerror(errno: i32) -> String {
    if errno >= 10000 {
        let text = std::io::Error::from_raw_os_error(errno).to_string();
        let text = text
            .strip_suffix(&format!(" (os error {errno})"))
            .unwrap_or(&text);
        return text
            .trim_end_matches(|c: char| c.is_whitespace() || c == '.')
            .to_string();
    }
    // SAFETY: the C runtime returns a pointer to a NUL-terminated message in a
    // per-thread buffer, valid until this thread's next `strerror`.
    let p = unsafe { libc::strerror(errno) };
    if p.is_null() {
        return format!("Unknown error {errno}");
    }
    // SAFETY: as above; copied out before anything else runs on this thread.
    unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned()
}

/// The `OSError` subclass CPython raises for `errno` (its errno numbers on
/// Windows); everything outside its fixed set stays `OSError`.
pub(crate) fn os_error_class(errno: i32) -> &'static str {
    match errno {
        EAGAIN | EALREADY | EINPROGRESS | EWOULDBLOCK => "BlockingIOError",
        EPIPE | ESHUTDOWN => "BrokenPipeError",
        ECHILD => "ChildProcessError",
        ECONNABORTED => "ConnectionAbortedError",
        ECONNREFUSED => "ConnectionRefusedError",
        ECONNRESET => "ConnectionResetError",
        EEXIST => "FileExistsError",
        ENOENT => "FileNotFoundError",
        EINTR => "InterruptedError",
        EISDIR => "IsADirectoryError",
        ENOTDIR => "NotADirectoryError",
        EACCES | EPERM => "PermissionError",
        ESRCH => "ProcessLookupError",
        ETIMEDOUT => "TimeoutError",
        _ => "OSError",
    }
}

/// Does `p` start with a drive (`C:`) or UNC (`\\server\share`) prefix —
/// `ntpath.splitdrive` finding a drive, either separator?
pub(crate) fn has_drive(p: &str) -> bool {
    matches!(Path::new(p).components().next(), Some(Component::Prefix(_)))
}

/// `PureWindowsPath.as_posix()`: every `\\` separator turned to `/`.
pub(crate) fn as_posix(p: &Path) -> String {
    p.to_string_lossy().replace('\\', "/")
}

// ── ntpath.realpath ─────────────────────────────────────────────────────────

/// python `ntpath.realpath(p, strict=False)`: the OS resolves the path
/// (`GetFinalPathNameByHandleW`, here `std::fs::canonicalize`); when it cannot,
/// the longest prefix it can is resolved and the rest joined on literally — a
/// dangling symlink followed to where it points. The `\\?\` prefix the OS hands
/// back is dropped when the path names the same file without it.
pub(crate) fn realpath(p: &Path) -> Result<PathBuf, String> {
    let had_prefix = is_verbatim(p);
    let path = if had_prefix {
        p.to_path_buf()
    } else {
        // `ntpath.abspath`: joined to the cwd, then normalized lexically.
        std::path::absolute(p)
            .map_err(|e| errno_of(&e).map_or_else(|| e.to_string(), |n| os_error_str(n, None)))?
    };
    let (path, initial) = match std::fs::canonicalize(&path) {
        Ok(real) => (real, None),
        Err(e) => (final_path_nonstrict(path), e.raw_os_error()),
    };
    if had_prefix || !is_verbatim(&path) {
        return Ok(path);
    }
    let stripped = strip_verbatim(&path);
    let keep = match std::fs::canonicalize(&stripped) {
        Ok(again) => again == path,
        // Missing now and missing at first: strip it anyway. "Missing" is any
        // code the nonstrict walk itself steps over (`unseen`), not only the
        // same one twice: another thread's save between the two looks — a folder
        // made (PATH_NOT_FOUND → FILE_NOT_FOUND), a file mid-replace (ACCESS_DENIED
        // ↔ FILE_NOT_FOUND) — changes the code, and the `\\?\` left on put the
        // path outside its root (python's one-threaded kernel never sees it).
        Err(e) => e.raw_os_error() == initial || (unseen(e.raw_os_error()) && unseen(initial)),
    };
    Ok(if keep { stripped } else { path })
}

/// The codes python's `_getfinalpathname_nonstrict` walks past — the path (or a
/// part of it) cannot be seen right now: missing, access denied (a file being
/// replaced, a folder being deleted), in use, a bad name, a network path down.
fn unseen(code: Option<i32>) -> bool {
    matches!(
        code,
        Some(1 | 2 | 3 | 5 | 21 | 32 | 50 | 53 | 65 | 67 | 87 | 123 | 161 | 1920 | 1921)
    )
}

/// python `ntpath._getfinalpathname_nonstrict`: walk up until the OS can
/// resolve a prefix, following a dangling link by hand on the way.
fn final_path_nonstrict(mut path: PathBuf) -> PathBuf {
    let mut tail: Vec<OsString> = Vec::new();
    let mut seen: Vec<PathBuf> = Vec::new();
    loop {
        if let Ok(real) = std::fs::canonicalize(&path) {
            return join_tail(real, &tail);
        }
        // A dangling link: followed by hand, and its TARGET resolved the same way
        // (its existing part by the OS) — a target written with an 8.3 short
        // name (`RUNNER~1`) must compare with the long one the root resolves to.
        // `seen` stops a loop: a link already followed is not followed again.
        let linked = readlink_deep(&path, &mut seen);
        if linked != path {
            path = linked;
            continue;
        }
        match (path.parent(), path.file_name()) {
            (Some(parent), Some(name)) => {
                tail.push(name.to_os_string());
                path = parent.to_path_buf();
            }
            // Down to the root, and the root does not resolve: keep it as it is.
            _ => return join_tail(path, &tail),
        }
    }
}

/// python `ntpath._readlink_deep`: follow `path` while it is a symlink; a
/// relative target resolves against the link's own directory.
fn readlink_deep(path: &Path, seen: &mut Vec<PathBuf>) -> PathBuf {
    let mut path = path.to_path_buf();
    loop {
        let key = PathBuf::from(path.to_string_lossy().to_lowercase());
        if seen.contains(&key) {
            return path;
        }
        seen.push(key);
        let is_link = std::fs::symlink_metadata(&path).is_ok_and(|m| m.file_type().is_symlink());
        if !is_link {
            return path;
        }
        let Ok(target) = std::fs::read_link(&path) else {
            return path;
        };
        path = if target.is_absolute() {
            target
        } else {
            let base = path.parent().map(Path::to_path_buf).unwrap_or_default();
            std::path::absolute(base.join(target)).unwrap_or(path)
        };
    }
}

fn join_tail(mut base: PathBuf, tail: &[OsString]) -> PathBuf {
    for name in tail.iter().rev() {
        base.push(name);
    }
    base
}

/// Does `p` start `\\?\` (a verbatim path)?
fn is_verbatim(p: &Path) -> bool {
    matches!(
        p.components().next(),
        Some(Component::Prefix(pre)) if pre.kind().is_verbatim()
    )
}

/// `\\?\C:\x` → `C:\x`, `\\?\UNC\server\share\x` → `\\server\share\x`.
fn strip_verbatim(p: &Path) -> PathBuf {
    let mut comps = p.components();
    let Some(Component::Prefix(pre)) = comps.next() else {
        return p.to_path_buf();
    };
    let head = match pre.kind() {
        Prefix::VerbatimDisk(d) => format!("{}:", d as char),
        Prefix::VerbatimUNC(server, share) => format!(
            r"\\{}\{}",
            server.to_string_lossy(),
            share.to_string_lossy()
        ),
        _ => return p.to_path_buf(),
    };
    let mut out = PathBuf::from(format!(r"{head}\"));
    for c in comps {
        if let Component::Normal(n) = c {
            out.push(n);
        }
    }
    out
}
