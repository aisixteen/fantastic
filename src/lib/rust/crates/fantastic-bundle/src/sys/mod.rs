//! sys — the OS boundary of the shared python-shape helpers.
//!
//! The C library's error text, the OS error → errno → `OSError` subclass map
//! ([`crate::oserror`]), and the path grammar `realpath` walks and
//! `is_absolute` reads ([`crate::realpath`]: `posixpath` on unix, `ntpath` on
//! Windows) — one module per OS family.

#[cfg(unix)]
mod unix;
#[cfg(unix)]
pub(crate) use unix::*;
#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub(crate) use windows::*;
