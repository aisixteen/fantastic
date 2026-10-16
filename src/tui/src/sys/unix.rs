//! The Unix side of [`crate::sys`]: descriptors, the controlling terminal, local
//! sockets and signals.

use std::fs::File;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::fs::PermissionsExt;

/// The controlling terminal, opened read-write to draw the TUI on.
pub(crate) const TTY: &str = "/dev/tty";
/// The sink that discards everything written to it.
pub(crate) const NULL_DEVICE: &str = "/dev/null";

/// Whether stdout is a terminal.
pub(crate) fn stdout_is_tty() -> bool {
    // SAFETY: isatty only inspects descriptor 1.
    unsafe { libc::isatty(1) != 0 }
}

/// Point stdout and stderr at `file`. The File may drop afterwards; the two
/// descriptors are independent duplicates.
pub(crate) fn redirect_stdio_to(file: &File) -> io::Result<()> {
    let fd = file.as_raw_fd();
    // SAFETY: `fd` is open for the life of this call, and 1/2 are valid targets.
    // dup2 closes the old descriptor atomically, so no write can slip between.
    unsafe {
        if libc::dup2(fd, 1) < 0 || libc::dup2(fd, 2) < 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

/// A File of its own on what stdout is now — kept before stdout is redirected.
pub(crate) fn dup_stdout() -> io::Result<File> {
    dup(1)
}

/// A File of its own on what stderr is now — kept before stderr is redirected.
pub(crate) fn dup_stderr() -> io::Result<File> {
    dup(2)
}

fn dup(fd: i32) -> io::Result<File> {
    // SAFETY: plain descriptor duplication of a descriptor open at process start.
    let d = unsafe { libc::dup(fd) };
    if d < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `d` was just duplicated and is owned by this File from here on.
    Ok(unsafe { File::from_raw_fd(d) })
}

/// A local socket: a connected stream, its listener and its two halves.
pub(crate) type LocalStream = tokio::net::UnixStream;
pub(crate) type LocalListener = tokio::net::UnixListener;
pub(crate) type LocalReadHalf = tokio::net::unix::OwnedReadHalf;
pub(crate) type LocalWriteHalf = tokio::net::unix::OwnedWriteHalf;

/// Connect to the local socket at `path`.
pub(crate) async fn connect_local(path: &str) -> io::Result<LocalStream> {
    LocalStream::connect(path).await
}

/// Bind the local socket at `path`.
pub(crate) fn bind_local(path: &str) -> io::Result<LocalListener> {
    LocalListener::bind(path)
}

/// Make the file at `path` readable and writable by this user only.
pub(crate) fn make_private(path: &str) -> io::Result<()> {
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
}

/// The termination request (SIGTERM).
pub(crate) fn terminate() -> io::Result<tokio::signal::unix::Signal> {
    tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
}

/// The interrupt (SIGINT).
pub(crate) fn interrupt() -> io::Result<tokio::signal::unix::Signal> {
    tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())
}
