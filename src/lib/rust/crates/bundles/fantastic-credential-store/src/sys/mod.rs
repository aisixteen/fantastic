//! The OS boundary of the credential store: the path grammar of the file
//! backend's absolute `file` (`~` expansion — the password database on unix,
//! the profile variables on Windows — and pathlib's parent/name split), and the
//! OS keychain behind the `keychain` backend — small named functions, one
//! module per OS family. The file backend's disk calls go through
//! `fantastic_file::fs`, not here.

mod keychain;
#[cfg(unix)]
mod unix;
#[cfg(windows)]
mod windows;

pub(crate) use keychain::*;
#[cfg(unix)]
pub(crate) use unix::*;
#[cfg(windows)]
pub(crate) use windows::*;
