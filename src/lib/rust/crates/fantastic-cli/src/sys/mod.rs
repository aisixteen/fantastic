//! The OS boundary of the headless host: the daemon's stop signals
//! ([`StopSignals`]) — SIGINT/SIGTERM/SIGHUP on unix, the console control
//! events on Windows — one module per OS family.

#[cfg(unix)]
mod unix;
#[cfg(unix)]
pub(crate) use unix::*;
#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub(crate) use windows::*;
