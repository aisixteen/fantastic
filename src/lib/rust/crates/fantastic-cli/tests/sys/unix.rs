//! Unix (macOS, Linux): signals.

use std::process::{Child, Command};

/// The stop [`stop`] sends, as the daemon's shutdown line names it.
pub const STOP_NAME: &str = "SIGTERM";

/// `cmd` as it is: a unix child is signalled by its pid alone.
pub fn stoppable(cmd: &mut Command) -> &mut Command {
    cmd
}

/// A live process of our own that outlives the test: `sleep 30`.
pub fn sleeper() -> Child {
    Command::new("/bin/sleep")
        .arg("30")
        .spawn()
        .expect("spawn a live pid")
}

/// SIGTERM — one of the three signals python installs a handler for.
pub fn stop(child: &Child) {
    // Safety: `child.id()` is a live pid this process owns; kill only delivers
    // the signal.
    unsafe {
        libc::kill(child.id() as libc::pid_t, libc::SIGTERM);
    }
}
