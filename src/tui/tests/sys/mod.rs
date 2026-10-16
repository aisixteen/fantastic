//! The integration tests' OS fixtures — the twin of the binary's `src/sys`: one
//! file per OS family, and the tests see only what they name.

#[cfg(unix)]
mod unix;
#[cfg(unix)]
pub use unix::*;
#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use windows::*;
