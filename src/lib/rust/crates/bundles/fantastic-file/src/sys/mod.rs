//! sys — the OS boundary of the clamped disk surface ([`crate::fs`]).
//!
//! Everything [`crate::fs`] needs beyond portable `std::fs`: path bytes for
//! the name sort, the exclusive no-follow create with a permission mode, the
//! no-follow private-file open and its `ELOOP` refusal, and the permission
//! bits / owner of a file against the current user.

#[cfg(unix)]
mod unix;
#[cfg(unix)]
pub(crate) use unix::*;
