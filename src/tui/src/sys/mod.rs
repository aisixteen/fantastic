//! THE OS SURFACE: every platform-specific call this binary makes, as one small
//! named function each. Call sites say `sys::…`; only this module knows the
//! platform — one file per OS family.

#[cfg(unix)]
mod unix;
#[cfg(unix)]
pub(crate) use unix::*;
#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub(crate) use windows::*;
