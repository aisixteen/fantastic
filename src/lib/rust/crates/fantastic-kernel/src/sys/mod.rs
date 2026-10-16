//! The OS boundary of the kernel: every call this crate makes into the
//! operating system — the workdir lock's `flock`, its file mode and file
//! identity, the pid liveness probe, the CSPRNG, and the symlink a test plants
//! — behind small named functions, one module per OS family.

#[cfg(unix)]
mod unix;
#[cfg(unix)]
pub(crate) use unix::*;
#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub(crate) use windows::*;
