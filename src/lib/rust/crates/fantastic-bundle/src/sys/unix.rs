//! The POSIX side of [`crate::sys`].

use std::collections::HashMap;
use std::ffi::{CStr, OsStr, OsString};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};

use crate::oserror::os_error_str;

/// The errno an OS error carries — on POSIX, the raw OS error itself.
pub(crate) fn errno_of(e: &std::io::Error) -> Option<i32> {
    e.raw_os_error()
}

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
fn os_bytes(s: &OsStr) -> &[u8] {
    s.as_bytes()
}

/// An owned OS string's raw bytes.
fn os_into_bytes(s: OsString) -> Vec<u8> {
    s.into_vec()
}

/// An OS string from raw bytes.
fn os_from_bytes(b: Vec<u8>) -> OsString {
    OsString::from_vec(b)
}

/// Does `p` start with a drive or UNC prefix? A POSIX path has none.
pub(crate) fn has_drive(_p: &str) -> bool {
    false
}

// ── posixpath.realpath ──────────────────────────────────────────────────────

/// python `posixpath.realpath(p, strict=False)`.
pub(crate) fn realpath(p: &Path) -> Result<PathBuf, String> {
    let mut seen: HashMap<Vec<u8>, Option<Vec<u8>>> = HashMap::new();
    let (path, _ok) = join_realpath(Vec::new(), os_bytes(p.as_os_str()).to_vec(), &mut seen);
    abspath(path)
}

/// python `posixpath._joinrealpath` — returns `(path, ok)`; `ok` is false when a
/// symlink loop stopped the walk, and the caller keeps the rest unresolved.
fn join_realpath(
    mut path: Vec<u8>,
    mut rest: Vec<u8>,
    seen: &mut HashMap<Vec<u8>, Option<Vec<u8>>>,
) -> (Vec<u8>, bool) {
    if rest.first() == Some(&b'/') {
        rest.remove(0);
        path = b"/".to_vec();
    }
    while !rest.is_empty() {
        let (name, tail) = partition(&rest, b'/');
        rest = tail;
        if name.is_empty() || name == b"." {
            continue;
        }
        if name == b".." {
            if path.is_empty() {
                path = b"..".to_vec();
            } else {
                let (head, last) = split(&path);
                path = head;
                if last == b".." {
                    path = join(&join(&path, b".."), b"..");
                }
            }
            continue;
        }
        let newpath = join(&path, &name);
        let is_link = std::fs::symlink_metadata(bytes_path(&newpath))
            .map(|m| m.file_type().is_symlink())
            .unwrap_or(false);
        if !is_link {
            path = newpath;
            continue;
        }
        match seen.get(&newpath) {
            Some(Some(resolved)) => {
                path = resolved.clone();
                continue;
            }
            // Seen but unresolved ⇒ a symlink loop: python returns the already
            // resolved part joined with the rest, unresolved.
            Some(None) => return (join(&newpath, &rest), false),
            None => {}
        }
        let Ok(link) = std::fs::read_link(bytes_path(&newpath)) else {
            path = newpath;
            continue;
        };
        seen.insert(newpath.clone(), None);
        let (walked, ok) = join_realpath(path, os_into_bytes(link.into_os_string()), seen);
        path = walked;
        if !ok {
            return (join(&path, &rest), false);
        }
        seen.insert(newpath, Some(path.clone()));
    }
    (path, true)
}

/// python `posixpath.abspath` — absolutize against the process cwd, then
/// `normpath`.
fn abspath(path: Vec<u8>) -> Result<PathBuf, String> {
    let path = if path.first() == Some(&b'/') {
        path
    } else {
        let cwd = std::env::current_dir().map_err(|e| {
            errno_of(&e).map_or_else(|| e.to_string(), |n| os_error_str(n, None))
        })?;
        join(os_bytes(cwd.as_os_str()), &path)
    };
    Ok(bytes_path(&normpath(&path)))
}

/// python `posixpath.normpath` — fold `//`, `.` and `..` lexically.
fn normpath(path: &[u8]) -> Vec<u8> {
    if path.is_empty() {
        return b".".to_vec();
    }
    // POSIX allows one or two leading slashes and treats three or more as one.
    let leading = if path.starts_with(b"//") && !path.starts_with(b"///") {
        2
    } else {
        usize::from(path.starts_with(b"/"))
    };
    let mut comps: Vec<&[u8]> = Vec::new();
    for comp in path.split(|b| *b == b'/') {
        if comp.is_empty() || comp == b"." {
            continue;
        }
        if comp != b".." || (leading == 0 && comps.is_empty()) || comps.last() == Some(&&b".."[..])
        {
            comps.push(comp);
        } else if !comps.is_empty() {
            comps.pop();
        }
    }
    let mut out = b"/".repeat(leading);
    out.extend_from_slice(&comps.join(&b'/'));
    if out.is_empty() {
        b".".to_vec()
    } else {
        out
    }
}

/// python `posixpath.join(a, b)`.
fn join(a: &[u8], b: &[u8]) -> Vec<u8> {
    if b.first() == Some(&b'/') {
        return b.to_vec();
    }
    let mut out = a.to_vec();
    if !out.is_empty() && out.last() != Some(&b'/') {
        out.push(b'/');
    }
    out.extend_from_slice(b);
    out
}

/// python `posixpath.split(p)`.
fn split(p: &[u8]) -> (Vec<u8>, Vec<u8>) {
    let i = p.iter().rposition(|b| *b == b'/').map_or(0, |i| i + 1);
    let (head, tail) = p.split_at(i);
    let mut head = head.to_vec();
    if !head.is_empty() && !head.iter().all(|b| *b == b'/') {
        while head.last() == Some(&b'/') {
            head.pop();
        }
    }
    (head, tail.to_vec())
}

/// python `str.partition(sep)` over bytes, returning `(before, after)`.
fn partition(s: &[u8], sep: u8) -> (Vec<u8>, Vec<u8>) {
    match s.iter().position(|b| *b == sep) {
        Some(i) => (s[..i].to_vec(), s[i + 1..].to_vec()),
        None => (s.to_vec(), Vec::new()),
    }
}

fn bytes_path(b: &[u8]) -> PathBuf {
    PathBuf::from(os_from_bytes(b.to_vec()))
}
