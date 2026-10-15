//! The OS boundary of the credential store: the password database behind `~`
//! expansion, and the OS keychain behind the `keychain` backend — small named
//! functions, one module per OS family. The file backend's disk calls go
//! through `fantastic_file::fs`, not here.

mod keychain;
#[cfg(unix)]
mod unix;

pub(crate) use keychain::*;
#[cfg(unix)]
pub(crate) use unix::*;
