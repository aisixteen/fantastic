//! sys — the OS boundary of the shared python-shape helpers.
//!
//! The C library's error text and the errno → `OSError` subclass map
//! ([`crate::oserror`]), and the byte view of paths that `realpath` walks
//! ([`crate::realpath`]).

#[cfg(unix)]
mod unix;
#[cfg(unix)]
pub(crate) use unix::*;
