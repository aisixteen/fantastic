//! CPython `os.path.realpath(p, strict=False)` — what `pathlib.Path.resolve()`
//! computes, and so what python's `fs` clamp compares against its root: every
//! existing component's symlink is FOLLOWED (a dangling one to where it points),
//! a missing tail is kept literal, and `..` is applied after the follow. A
//! lexical compare would admit an outward symlink the OS then follows at open
//! time. ONE copy: `fantastic-file`'s `fs` clamp resolves through this, and so
//! does the kernel's cold tree walk (the substrate cannot depend on a bundle), as
//! python's walks go through `file_bridge.fs`.

use std::collections::HashMap;
use std::ffi::OsString;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};

use crate::oserror::os_error_str;

/// python `os.path.realpath(p, strict=False)` — the canonical path, with every
/// symlink it can follow followed and a missing tail normalized.
///
/// # Errors
/// A relative `p` absolutizes against the process cwd, and python's
/// `os.getcwd()` raises when the cwd is gone — its `OSError` text
/// (`[Errno 2] No such file or directory`).
pub fn realpath(p: &Path) -> Result<PathBuf, String> {
    let mut seen: HashMap<Vec<u8>, Option<Vec<u8>>> = HashMap::new();
    let (path, _ok) = join_realpath(Vec::new(), p.as_os_str().as_bytes().to_vec(), &mut seen);
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
        let (walked, ok) = join_realpath(path, link.into_os_string().into_vec(), seen);
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
            e.raw_os_error()
                .map_or_else(|| e.to_string(), |n| os_error_str(n, None))
        })?;
        join(cwd.as_os_str().as_bytes(), &path)
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
    PathBuf::from(OsString::from_vec(b.to_vec()))
}
