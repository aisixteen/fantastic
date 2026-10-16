//! CPython `os.path.realpath(p, strict=False)` — what `pathlib.Path.resolve()`
//! computes, and so what python's `fs` clamp compares against its root: every
//! existing component's symlink is FOLLOWED (a dangling one to where it points),
//! a missing tail is kept literal, and `..` is applied after the follow. A
//! lexical compare would admit an outward symlink the OS then follows at open
//! time. ONE copy: `fantastic-file`'s `fs` clamp resolves through this, and so
//! does the kernel's cold tree walk (the substrate cannot depend on a bundle), as
//! python's walks go through `file_bridge.fs`. [`is_absolute`] is what both
//! refuse before resolving anything.

use std::path::{Path, PathBuf};

use crate::sys;

/// python `os.path.realpath(p, strict=False)` — the canonical path, with every
/// symlink it can follow followed and a missing tail normalized. The path
/// grammar is the OS family's (`posixpath` / `ntpath`), so the walk lives in
/// [`crate::sys`].
///
/// # Errors
/// A relative `p` absolutizes against the process cwd, and python's
/// `os.getcwd()` raises when the cwd is gone — its `OSError` text
/// (`[Errno 2] No such file or directory`).
pub fn realpath(p: &Path) -> Result<PathBuf, String> {
    sys::realpath(p)
}

/// Is a request path ABSOLUTE — refused, never re-rooted under a root? A
/// `/`-prefixed path on every OS (a wire path is `/`-separated); on Windows also
/// one with a drive (`C:`) or UNC (`\\server\share`) prefix ([`crate::sys`]).
#[must_use]
pub fn is_absolute(p: &str) -> bool {
    p.starts_with('/') || sys::has_drive(p)
}
