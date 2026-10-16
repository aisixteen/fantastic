//! Windows: console control events.

use std::os::windows::process::CommandExt;
use std::process::{Child, Command, Stdio};

use windows_sys::Win32::System::Console::{GenerateConsoleCtrlEvent, CTRL_BREAK_EVENT};
use windows_sys::Win32::System::Threading::CREATE_NEW_PROCESS_GROUP;

/// The stop [`stop`] sends, as the daemon's shutdown line names it (the C
/// runtime's name for Ctrl-Break).
pub const STOP_NAME: &str = "SIGBREAK";

/// `cmd` in a process group of its own, so a Ctrl-Break reaches the daemon
/// and not this test process too.
pub fn stoppable(cmd: &mut Command) -> &mut Command {
    cmd.creation_flags(CREATE_NEW_PROCESS_GROUP)
}

/// A live process of our own that outlives the test: a 30 s `ping` of the
/// loopback (Windows has no `sleep` binary).
pub fn sleeper() -> Child {
    Command::new("ping")
        .args(["-n", "31", "127.0.0.1"])
        .stdout(Stdio::null())
        .spawn()
        .expect("spawn a live pid")
}

/// Ctrl-Break to the daemon's process group (its pid names it).
pub fn stop(child: &Child) {
    // SAFETY: a plain console call; the group is the child spawned
    // [`stoppable`].
    assert_ne!(
        unsafe { GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT, child.id()) },
        0,
        "Ctrl-Break the daemon: {}",
        std::io::Error::last_os_error()
    );
}
