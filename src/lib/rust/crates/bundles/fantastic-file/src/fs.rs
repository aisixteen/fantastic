//! fs — THE single clamped disk surface, mirror of python
//! `file_bridge/fs.py`.
//!
//! This module is the ONLY place this runtime opens a path that came from a
//! caller. The running-dir law lives here (a `root` is clamped to the running
//! dir; a `path` is clamped to that root — `../`, absolute paths and outward
//! symlinks all refuse), so the clamp is **unbypassable** for every path a caller
//! names: nothing reads or writes one outside its allowed root by going around
//! this module. The cold bring-up read `static_loader` does before any
//! file_bridge agent exists is not exempt — it calls [`resolve`] here, as
//! python's `static_loader` imports `file_bridge.fs`. Nor is `credential_store`'s
//! `file` backend — secrets at an ABSOLUTE path outside the running dir by
//! design, written 0600 ([`write_bytes`]) and read through ONE `O_NOFOLLOW` open
//! judged on its own descriptor ([`read_private`]).
//!
//! The ONLY disk code outside this module (none of it opens a caller-named path):
//! - the substrate crate `fantastic-kernel`, which cannot depend on a bundle:
//!   `bootstrap.rs` (creates `.fantastic`, reads the root record, writes the
//!   virgin seed's root record), `persistence.rs` (`walk_tree` — the cold tree
//!   read, also behind a loader's `disk_parents`; `read_record_at` /
//!   `write_record_at`) and `lock.rs` (`lock.yaml`);
//! - [`fantastic_bundle::realpath`] — the ONE copy of python's `os.path.realpath`,
//!   shared by this module's clamp and the kernel's cold walk (`persistence.rs`
//!   `clamp`): `lstat` + `readlink` per component, and `getcwd` for a relative
//!   path — metadata only, it opens nothing;
//! - the `fantastic-cli` binary's `.env` read (`load_dotenv`);
//! - `fantastic-exec-bridge`'s process plumbing: `spawn::all_pids` lists `/proc`
//!   (Linux) for a job's session members; `spawn::resolve` probes the child's
//!   `PATH` dirs for a bare command (`is_file` + `access` — metadata only; the OS
//!   opens what it execs); `pty::mkdtemp` creates a PTY job's paste scratch dir
//!   in the system temp dir — the image itself is written through here;
//! - `fantastic-file`'s `list`, which stats the entries [`list_dir`] returned —
//!   paths this module already clamped.
//!
//! Resolution is python's: `pathlib.Path.resolve(strict=False)`, i.e.
//! `os.path.realpath` ([`fantastic_bundle::realpath`]) — every existing
//! component's symlink is FOLLOWED, a missing tail is kept literal, and `..` is
//! applied after the follow. A lexical compare would admit an outward symlink the
//! OS then follows at open time.
//!
//! Every function takes `(root, path)`: `root` is the clamp boundary, `path` is
//! relative to it. Errors are the text python's `ValueError`/`OSError` carry, so
//! a caller renders `str(e)` by forwarding the string.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use fantastic_bundle::{
    io_error_str, is_absolute, os_error_str2, py_repr, realpath, utf8_decode_error_str,
};

use crate::sys;

/// python `fs.resolve_root` — clamp `root` to the running dir `base` (the
/// kernel's [`fantastic_kernel::Kernel::workdir`], python's `fs.Root.base` — the
/// ONE source). A relative root resolves under `base`; an absolute root is
/// allowed only if it lies inside `base`; `..`/symlink escapes refuse.
///
/// # Errors
/// `fs: root '<raw>' escapes the running dir` — python's `ValueError` text; the
/// cwd's `OSError` text when a relative path cannot absolutize ([`realpath`]).
pub fn resolve_root(root: &str, base: &Path) -> Result<PathBuf, String> {
    let base = realpath(base)?;
    let joined = if is_absolute(root) {
        PathBuf::from(root)
    } else {
        base.join(root)
    };
    let resolved = realpath(&joined)?;
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
/// `fs: path '<path>' escapes root` — python's `ValueError` text; the cwd's
/// `OSError` text when a relative root cannot absolutize ([`realpath`]).
pub fn resolve(root: &Path, path: &str) -> Result<PathBuf, String> {
    let root_p = realpath(root)?;
    if is_absolute(path) {
        // An absolute path is a hard escape — refused, never silently re-rooted
        // (a stripped slash would serve `/etc/passwd` as `root/etc/passwd`).
        return Err(format!("fs: path {} escapes root", py_repr(path)));
    }
    let target = if path.is_empty() {
        root_p.clone()
    } else {
        realpath(&root_p.join(path))?
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
        // A FRESH tmp per write, created exclusively and never through a link: a
        // fixed `<name>.tmp` would follow whatever symlink sat at that name (a
        // write escaping the root) and be shared by concurrent writers, tearing
        // the target.
        let mut name = target.file_name().unwrap_or_default().to_os_string();
        name.push(format!(
            ".tmp.{}",
            fantastic_kernel::lifecycle::token_hex(4)
        ));
        let tmp = target.with_file_name(name);
        if let Err(e) = write_new_with_mode(&tmp, data, mode) {
            let _ = sys::unlink(&tmp);
            return Err(e);
        }
        std::fs::rename(&tmp, &target).map_err(|e| io_error_str(&e, &target))
    } else {
        if mode.is_some() {
            return Err("fs: `mode` needs an atomic write".to_string());
        }
        std::fs::write(&target, data).map_err(|e| io_error_str(&e, &target))
    }
}

/// python's `os.open(tmp, O_WRONLY|O_CREAT|O_EXCL|O_NOFOLLOW, mode or 0o666)` →
/// `os.fchmod(fd, mode)` (a mode given) → `os.write(fd, data)`; the create and
/// its mode are [`sys::create_new_nofollow`].
fn write_new_with_mode(tmp: &Path, data: &[u8], mode: Option<u32>) -> Result<(), String> {
    use std::io::Write;
    let mut f = sys::create_new_nofollow(tmp, mode).map_err(|e| io_error_str(&e, tmp))?;
    f.write_all(data).map_err(|e| io_error_str(&e, tmp))
}

/// What [`read_private`] refused, nothing read: the entry it found is not a
/// private file. `mode` (its permission bits) and `uid` (its owner) say what it
/// IS, for the caller's own refusal text. Python `fs.NotPrivate`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NotPrivate {
    /// The leaf that was opened (or the symlink standing in its place).
    pub path: PathBuf,
    /// Its permission bits (`S_IMODE`; on Windows synthesized from its DACL).
    pub mode: u32,
    /// Its owner (on Windows the owner SID's RID).
    pub uid: u32,
}

impl std::fmt::Display for NotPrivate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "fs: {} is not a private file (mode 0o{:o}, uid {}) — refused, nothing read",
            self.path.display(),
            self.mode,
            self.uid
        )
    }
}

/// Why [`read_private`] read nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PrivateReadError {
    /// The entry is not a private file — python's `NotPrivate(PermissionError)`.
    NotPrivate(NotPrivate),
    /// The clamp's text, or python's `OSError` text (`FileNotFoundError` when
    /// absent).
    Os(String),
}

/// python `fs.read_private` — read a PRIVATE file (a regular file owned by this
/// user with no group/other permission bits) through ONE open. `O_NOFOLLOW`
/// refuses a symlink in the leaf's place, and the OPEN DESCRIPTOR is what is
/// judged (`fstat`) and read, so nothing can swap the file between the check
/// and the read (an `lstat` of the path and then a read of the path again is a
/// window). `O_NONBLOCK` so a FIFO planted there cannot
/// hang the open — it is judged and refused like any other non-regular entry.
/// On Windows the same one open judges the handle's owner and DACL
/// ([`sys::open_private`]).
///
/// # Errors
/// [`PrivateReadError::NotPrivate`] (nothing read) when the entry is not
/// private; [`PrivateReadError::Os`] when it is absent or unreadable.
pub fn read_private(root: &Path, path: &str) -> Result<Vec<u8>, PrivateReadError> {
    use std::io::Read;
    let leaf = lexical_leaf(root, path).map_err(PrivateReadError::Os)?;
    let opened =
        sys::open_private(&leaf).map_err(|e| PrivateReadError::Os(io_error_str(&e, &leaf)))?;
    let mut f = match opened {
        sys::Private::File(f) => f,
        sys::Private::Not { mode, uid } => {
            return Err(PrivateReadError::NotPrivate(NotPrivate {
                path: leaf,
                mode,
                uid,
            }));
        }
    };
    let mut out = Vec::new();
    f.read_to_end(&mut out)
        .map_err(|e| PrivateReadError::Os(io_error_str(&e, &leaf)))?;
    Ok(out)
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
    // share a parent, so this is the order of their names.
    out.sort_by(|a, b| sys::path_order(a, b));
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
    std::fs::rename(&o, &n).map_err(|e| match fantastic_bundle::errno_of(&e) {
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
        return sys::unlink(&leaf).map_err(|e| io_error_str(&e, &leaf));
    }
    let target = resolve(root, path)?;
    if target.is_dir() {
        if recursive {
            rmtree(&target)
        } else {
            std::fs::remove_dir(&target).map_err(|e| io_error_str(&e, &target))
        }
    } else {
        sys::unlink(&target).map_err(|e| io_error_str(&e, &target))
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
            sys::unlink(&sub).map_err(|e| io_error_str(&e, &sub))?;
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
