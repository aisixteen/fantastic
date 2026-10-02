//! fs — THE single clamped disk surface, mirror of python
//! `file_bridge/fs.py`.
//!
//! This module is the ONLY place this runtime opens a path that came from a
//! caller. The running-dir law lives here (a `root` is clamped to the running
//! dir; a `path` is clamped to that root — `../`, absolute paths and outward
//! symlinks all refuse), so the clamp is **unbypassable**: nothing reads or
//! writes outside its allowed root by going around it, because there is no other
//! disk code to go around to. The cold bring-up read `static_loader` does before
//! any file_bridge agent exists is not exempt — it calls [`resolve`] here, as
//! python's `static_loader` imports `file_bridge.fs`.
//!
//! Resolution is python's: `pathlib.Path.resolve(strict=False)`, i.e.
//! `os.path.realpath` — every existing component's symlink is FOLLOWED, a
//! missing tail is kept literal, and `..` is applied after the follow. A lexical
//! compare would admit an outward symlink the OS then follows at open time.
//!
//! Every function takes `(root, path)`: `root` is the clamp boundary, `path` is
//! relative to it. Errors are the text python's `ValueError`/`OSError` carry, so
//! a caller renders `str(e)` by forwarding the string.

use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};

use fantastic_bundle::{io_error_str, os_error_str2, py_repr, utf8_decode_error_str};

/// python `os.path.realpath(p, strict=False)` — the canonical path, with every
/// symlink it can follow followed and a missing tail normalized, never raising.
#[must_use]
pub fn realpath(p: &Path) -> PathBuf {
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
fn abspath(path: Vec<u8>) -> PathBuf {
    let path = if path.first() == Some(&b'/') {
        path
    } else {
        let cwd = std::env::current_dir().unwrap_or_default();
        join(cwd.as_os_str().as_bytes(), &path)
    };
    bytes_path(&normpath(&path))
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

/// python `fs.resolve_root` — clamp `root` to the running dir `base` (the
/// kernel's [`fantastic_kernel::Kernel::workdir`], python's `fs.Root.base` — the
/// ONE source). A relative root resolves under `base`; an absolute root is
/// allowed only if it lies inside `base`; `..`/symlink escapes refuse.
///
/// # Errors
/// `fs: root '<raw>' escapes the running dir` — python's `ValueError` text.
pub fn resolve_root(root: &str, base: &Path) -> Result<PathBuf, String> {
    let base = realpath(base);
    let joined = if root.starts_with('/') {
        PathBuf::from(root)
    } else {
        base.join(root)
    };
    let resolved = realpath(&joined);
    if !resolved.starts_with(&base) {
        return Err(format!(
            "fs: root {} escapes the running dir",
            py_repr(root)
        ));
    }
    Ok(resolved)
}

/// python `fs.resolve` — clamp `path` within `root` (itself already the clamped
/// root). The target may not escape via `..`, an absolute path or a symlink,
/// dangling or not.
///
/// # Errors
/// `fs: path '<path>' escapes root` — python's `ValueError` text.
pub fn resolve(root: &Path, path: &str) -> Result<PathBuf, String> {
    let root_p = realpath(root);
    if path.starts_with('/') {
        // An absolute path is a hard escape — refused, never silently re-rooted
        // (a stripped slash would serve `/etc/passwd` as `root/etc/passwd`).
        return Err(format!("fs: path {} escapes root", py_repr(path)));
    }
    let target = if path.is_empty() {
        root_p.clone()
    } else {
        realpath(&root_p.join(path))
    };
    if !target.starts_with(&root_p) {
        return Err(format!("fs: path {} escapes root", py_repr(path)));
    }
    Ok(target)
}

/// python `fs._lexical_leaf` — the UNRESOLVED leaf under a resolved parent, the
/// path a symlink itself lives at. [`resolve`] follows a link all the way to its
/// target, so the leaf must be probed before the follow.
///
/// # Errors
/// The parent is still clamped, so the leaf cannot escape.
pub fn lexical_leaf(root: &Path, path: &str) -> Result<PathBuf, String> {
    let (parent_rel, name) = match path.rfind('/') {
        Some(i) => (&path[..i], &path[i + 1..]),
        None => ("", path),
    };
    let parent = resolve(root, parent_rel)?;
    Ok(if name.is_empty() {
        parent
    } else {
        parent.join(name)
    })
}

/// python `fs.is_symlink` — true if `path` ITSELF is a symlink (dangling or
/// not); `exists()` follows links, so a dangling one reports absent while still
/// occupying the name.
///
/// # Errors
/// The clamp's text, when the leaf's parent escapes the root.
pub fn is_symlink(root: &Path, path: &str) -> Result<bool, String> {
    Ok(std::fs::symlink_metadata(lexical_leaf(root, path)?)
        .map(|m| m.file_type().is_symlink())
        .unwrap_or(false))
}

/// python `fs.read_text` — whole-file utf-8 read.
///
/// # Errors
/// python's `OSError` text, or its `UnicodeDecodeError` text for non-utf-8 bytes.
pub fn read_text(root: &Path, path: &str) -> Result<String, String> {
    let target = resolve(root, path)?;
    let bytes = std::fs::read(&target).map_err(|e| io_error_str(&e, &target))?;
    String::from_utf8(bytes).map_err(|e| utf8_decode_error_str(e.as_bytes(), &e.utf8_error()))
}

/// python `fs.read_bytes` — the whole file.
///
/// # Errors
/// python's `OSError` text.
pub fn read_bytes(root: &Path, path: &str) -> Result<Vec<u8>, String> {
    let target = resolve(root, path)?;
    std::fs::read(&target).map_err(|e| io_error_str(&e, &target))
}

/// python `fs.read_chunk` — one chunk at `offset`, plus the file's total size.
/// The buffer grows only to what EXISTS (`take(length)`), never an up-front
/// allocation a caller-chosen 1 PB `length` would abort the kernel on.
///
/// # Errors
/// python's `OSError` text.
pub fn read_chunk(
    root: &Path,
    path: &str,
    offset: u64,
    length: u64,
) -> Result<(Vec<u8>, u64), String> {
    use std::io::{Read, Seek, SeekFrom};
    let target = resolve(root, path)?;
    let total = std::fs::metadata(&target)
        .map_err(|e| io_error_str(&e, &target))?
        .len();
    let mut f = std::fs::File::open(&target).map_err(|e| io_error_str(&e, &target))?;
    f.seek(SeekFrom::Start(offset))
        .map_err(|e| io_error_str(&e, &target))?;
    let mut buf = Vec::new();
    f.take(length)
        .read_to_end(&mut buf)
        .map_err(|e| io_error_str(&e, &target))?;
    Ok((buf, total))
}

/// python `fs.write_chunk` — write one chunk at `offset` (`None` ⇒ append at
/// end). `truncate` (or a first write to a new file) starts fresh. Returns
/// `(offset_written, new_size)`.
///
/// # Errors
/// python's `OSError` text.
pub fn write_chunk(
    root: &Path,
    path: &str,
    data: &[u8],
    offset: Option<u64>,
    truncate: bool,
) -> Result<(u64, u64), String> {
    use std::io::{Seek, SeekFrom, Write};
    let target = resolve(root, path)?;
    mkdir_parents(&target)?;
    let exists = target.exists();
    if truncate || !exists {
        std::fs::write(&target, b"").map_err(|e| io_error_str(&e, &target))?;
    }
    let off = match offset {
        Some(o) => o,
        None => std::fs::metadata(&target)
            .map_err(|e| io_error_str(&e, &target))?
            .len(),
    };
    let mut f = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&target)
        .map_err(|e| io_error_str(&e, &target))?;
    f.seek(SeekFrom::Start(off))
        .map_err(|e| io_error_str(&e, &target))?;
    f.write_all(data).map_err(|e| io_error_str(&e, &target))?;
    let size = std::fs::metadata(&target)
        .map_err(|e| io_error_str(&e, &target))?
        .len();
    Ok((off, size))
}

/// python `fs.write_bytes` — whole-file write, parent dirs created. `atomic`
/// commits through a sibling `.tmp` + rename; a non-atomic write is the
/// in-place one a streaming sink wants.
///
/// `mode` (atomic only) creates the file with exactly those permission bits — a
/// 0600 secret file is never readable by others, not even between create and
/// chmod: the `.tmp` is created `O_EXCL` with the mode, then `fchmod`ed to it
/// (the umask may have narrowed it; never widened).
///
/// # Errors
/// python's `OSError` text, the clamp's `ValueError` text, or
/// `fs: `mode` needs an atomic write` for a mode on a non-atomic write.
pub fn write_bytes(
    root: &Path,
    path: &str,
    data: &[u8],
    atomic: bool,
    mode: Option<u32>,
) -> Result<(), String> {
    let target = resolve(root, path)?;
    mkdir_parents(&target)?;
    if atomic {
        let mut name = target.file_name().unwrap_or_default().to_os_string();
        name.push(".tmp");
        let tmp = target.with_file_name(name);
        match mode {
            None => std::fs::write(&tmp, data).map_err(|e| io_error_str(&e, &tmp))?,
            Some(mode) => write_new_with_mode(&tmp, data, mode)?,
        }
        std::fs::rename(&tmp, &target).map_err(|e| io_error_str(&e, &target))
    } else {
        if mode.is_some() {
            return Err("fs: `mode` needs an atomic write".to_string());
        }
        std::fs::write(&target, data).map_err(|e| io_error_str(&e, &target))
    }
}

/// python's `tmp.unlink(missing_ok=True)` → `os.open(tmp, O_WRONLY|O_CREAT|O_EXCL,
/// mode)` → `os.fchmod(fd, mode)` → `os.write(fd, data)`.
fn write_new_with_mode(tmp: &Path, data: &[u8], mode: u32) -> Result<(), String> {
    use std::io::Write;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    match std::fs::remove_file(tmp) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
            return Err(io_error_str(&e, tmp));
        }
        _ => {}
    }
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(mode)
        .open(tmp)
        .map_err(|e| io_error_str(&e, tmp))?;
    f.set_permissions(std::fs::Permissions::from_mode(mode))
        .map_err(|e| io_error_str(&e, tmp))?;
    f.write_all(data).map_err(|e| io_error_str(&e, tmp))
}

/// python `fs.lstat` — the entry's OWN metadata (a symlink is not followed), for
/// a caller that must check what a path IS (type, owner, permission bits) before
/// trusting it.
///
/// # Errors
/// The clamp's text, or python's `OSError` text.
pub fn lstat(root: &Path, path: &str) -> Result<std::fs::Metadata, String> {
    let leaf = lexical_leaf(root, path)?;
    std::fs::symlink_metadata(&leaf).map_err(|e| io_error_str(&e, &leaf))
}

/// python `fs.list_dir` — the dir's entries as resolved paths, sorted by name
/// (raw: exposure is the AGENT's `allow` list, not a disk concern).
///
/// # Errors
/// python's `OSError` text when the walk fails.
pub fn list_dir(root: &Path, path: &str) -> Result<Vec<PathBuf>, String> {
    let target = resolve(root, path)?;
    let mut out: Vec<PathBuf> = Vec::new();
    for entry in std::fs::read_dir(&target).map_err(|e| io_error_str(&e, &target))? {
        out.push(entry.map_err(|e| io_error_str(&e, &target))?.path());
    }
    // python sorts `Path` objects, which compare by their parts — all entries
    // share a parent, so this is the name's code points, i.e. utf-8 bytes.
    out.sort_by(|a, b| a.as_os_str().as_bytes().cmp(b.as_os_str().as_bytes()));
    Ok(out)
}

/// python `fs.mkdir` — `Path.mkdir(parents=True, exist_ok=True)`.
///
/// # Errors
/// python's `OSError` text, naming the path whose `mkdir` failed.
pub fn mkdir(root: &Path, path: &str) -> Result<(), String> {
    let target = resolve(root, path)?;
    mkdir_p(&target)
}

/// python `Path.mkdir(parents=True, exist_ok=True)`: try the leaf, create the
/// parent on ENOENT and retry, and accept an existing DIRECTORY only.
fn mkdir_p(target: &Path) -> Result<(), String> {
    match std::fs::create_dir(target) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let Some(parent) = target.parent().filter(|p| p != &target) else {
                return Err(io_error_str(&e, target));
            };
            mkdir_p(parent)?;
            match std::fs::create_dir(target) {
                Ok(()) => Ok(()),
                Err(e) => exists_as_dir(&e, target),
            }
        }
        Err(e) => exists_as_dir(&e, target),
    }
}

/// `exist_ok=True` accepts an existing DIRECTORY and nothing else — a file under
/// the name keeps python's `FileExistsError`.
fn exists_as_dir(e: &std::io::Error, target: &Path) -> Result<(), String> {
    if target.is_dir() {
        Ok(())
    } else {
        Err(io_error_str(e, target))
    }
}

/// The parent-dir creation every write does first (`target.parent.mkdir`).
fn mkdir_parents(target: &Path) -> Result<(), String> {
    match target.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => mkdir_p(parent),
        _ => Ok(()),
    }
}

/// python `fs.rename` — both ends clamped, new parents created.
///
/// # Errors
/// python's `OSError` text, in its two-filename form.
pub fn rename(root: &Path, old: &str, new: &str) -> Result<(), String> {
    let o = resolve(root, old)?;
    let n = resolve(root, new)?;
    mkdir_parents(&n)?;
    std::fs::rename(&o, &n).map_err(|e| match e.raw_os_error() {
        Some(errno) => os_error_str2(errno, &o.to_string_lossy(), &n.to_string_lossy()),
        None => e.to_string(),
    })
}

/// python `fs.remove` — delete a file, an empty dir, or (recursive) a dir tree.
///
/// A symlink is UNLINKED, never followed: the leaf is probed before `resolve()`
/// follows it, so deleting a link leaves its target alone and a dangling link is
/// still a deletable name.
///
/// # Errors
/// python's `OSError` text.
pub fn remove(root: &Path, path: &str, recursive: bool) -> Result<(), String> {
    let leaf = lexical_leaf(root, path)?;
    if std::fs::symlink_metadata(&leaf).is_ok_and(|m| m.file_type().is_symlink()) {
        return std::fs::remove_file(&leaf).map_err(|e| io_error_str(&e, &leaf));
    }
    let target = resolve(root, path)?;
    if target.is_dir() {
        if recursive {
            rmtree(&target)
        } else {
            std::fs::remove_dir(&target).map_err(|e| io_error_str(&e, &target))
        }
    } else {
        std::fs::remove_file(&target).map_err(|e| io_error_str(&e, &target))
    }
}

/// python `fs._rmtree` — recurse into real dirs, unlink everything else, then
/// `rmdir` the dir itself, so every failure names the path python would name.
fn rmtree(target: &Path) -> Result<(), String> {
    for entry in std::fs::read_dir(target).map_err(|e| io_error_str(&e, target))? {
        let sub = entry.map_err(|e| io_error_str(&e, target))?.path();
        let is_real_dir = std::fs::symlink_metadata(&sub)
            .map(|m| m.is_dir() && !m.file_type().is_symlink())
            .unwrap_or(false);
        if is_real_dir {
            rmtree(&sub)?;
        } else {
            std::fs::remove_file(&sub).map_err(|e| io_error_str(&e, &sub))?;
        }
    }
    std::fs::remove_dir(target).map_err(|e| io_error_str(&e, target))
}

/// python `Path.exists()` — follows symlinks, so a dangling one reports absent.
///
/// # Errors
/// The clamp's text.
pub fn exists(root: &Path, path: &str) -> Result<bool, String> {
    Ok(resolve(root, path)?.exists())
}

/// python `Path.is_dir()` — follows symlinks.
///
/// # Errors
/// The clamp's text.
pub fn is_dir(root: &Path, path: &str) -> Result<bool, String> {
    Ok(resolve(root, path)?.is_dir())
}

/// python `Path.is_file()` — follows symlinks.
///
/// # Errors
/// The clamp's text.
pub fn is_file(root: &Path, path: &str) -> Result<bool, String> {
    Ok(resolve(root, path)?.is_file())
}

/// The name of one entry, as `Path.name` gives it.
#[must_use]
pub fn file_name(p: &Path) -> String {
    p.file_name()
        .unwrap_or_else(|| OsStr::new(""))
        .to_string_lossy()
        .into_owned()
}
