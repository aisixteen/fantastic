//! The OS boundary of the binary's integration tests: spawning the daemon so it
//! can be stopped alone, and stopping it — SIGTERM on unix, a console Ctrl-Break
//! on Windows — with the name its shutdown line gives the stop; and a live
//! process to stand for a pid that holds no lock.

#[cfg(unix)]
mod unix;
#[cfg(unix)]
pub use unix::*;
#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use windows::*;
