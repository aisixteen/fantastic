//! THE OS SURFACE: every platform-specific call this binary makes, as one small
//! named function each. Call sites say `sys::…`; only this module knows the
//! platform. Unix is the one side; a Windows side lands here when it is written.

#[cfg(unix)]
mod unix;
#[cfg(unix)]
pub(crate) use unix::*;
