//! The OS boundary of exec_bridge: process spawning and teardown (sessions,
//! signals, the pid listing), the inherited control socket, pseudo-terminals
//! (open, window size, raw reads/writes on the master), and the temp dir a pasted
//! image lands in. Every OS call the rest of the crate makes goes through here.

#[cfg(unix)]
mod unix;
#[cfg(unix)]
pub(crate) use unix::*;
