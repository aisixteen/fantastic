//! The Unix side of [`crate::sys`]: descriptors, the controlling terminal, local
//! sockets, signals, the user's shell and the process table.

use std::fs::{File, OpenOptions};
use std::io;
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::fs::PermissionsExt;

/// The controlling terminal the TUI draws on.
pub(crate) const TTY: &str = "/dev/tty";
/// The sink that discards everything written to it.
pub(crate) const NULL_DEVICE: &str = "/dev/null";
/// The variable naming the user's shell — what the terminal runs.
pub(crate) const SHELL_VAR: &str = "SHELL";

/// The controlling terminal, opened read-write to draw the TUI on.
pub(crate) fn open_tty() -> io::Result<File> {
    OpenOptions::new().read(true).write(true).open(TTY)
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

/// What [`terminate`] answers: each SIGTERM, from `recv`.
pub(crate) type Terminate = tokio::signal::unix::Signal;
/// What [`interrupt`] answers: each SIGINT, from `recv`.
pub(crate) type Interrupt = tokio::signal::unix::Signal;

/// The termination request (SIGTERM).
pub(crate) fn terminate() -> io::Result<Terminate> {
    tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
}

/// The interrupt (SIGINT).
pub(crate) fn interrupt() -> io::Result<Interrupt> {
    tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())
}

/// `fantastic __ctrl_break <pid>`: the stop helper exists on Windows only.
pub(crate) fn ctrl_break_main(_argv: &[String]) -> ! {
    eprintln!("fantastic: ctrl-break: Windows only");
    std::process::exit(2)
}

/// Every process, one `pid ppid pgid tpgid args` row each
/// (`ps -A -o pid=,ppid=,pgid=,tpgid=,args=`): where a shell's foreground
/// command is read from (`kern/term_watch`).
pub(crate) fn process_table() -> Option<String> {
    let out = std::process::Command::new("ps").args(["-A", "-o", "pid=,ppid=,pgid=,tpgid=,args="]).output().ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

/// `READY=1` to systemd (`sd_notify(3)`), when `NOTIFY_SOCKET` names its socket
/// (a `Type=notify` unit): one datagram to a path (`/…`) or, on Linux, the
/// abstract namespace (`@…`). Unset: nothing. A failed send is said on stderr,
/// and systemd's start timeout fails the unit.
pub(crate) fn notify_ready() {
    let Some(addr) = std::env::var_os("NOTIFY_SOCKET").filter(|v| !v.is_empty()) else { return };
    if let Err(e) = notify_to(&addr) {
        eprintln!("fantastic: NOTIFY_SOCKET: {e}");
    }
}

#[cfg(target_os = "linux")]
fn notify_to(addr: &std::ffi::OsStr) -> io::Result<()> {
    use std::os::linux::net::SocketAddrExt;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::net::{SocketAddr, UnixDatagram};
    let bytes = addr.as_bytes();
    let to = match bytes.first() {
        Some(b'/') => SocketAddr::from_pathname(addr)?,
        Some(b'@') => SocketAddr::from_abstract_name(&bytes[1..])?,
        _ => return Err(io::Error::new(io::ErrorKind::InvalidInput, "neither a path nor an abstract address")),
    };
    UnixDatagram::unbound()?.send_to_addr(b"READY=1", &to)?;
    Ok(())
}

/// Only systemd sets `NOTIFY_SOCKET`: another Unix has nothing to tell.
#[cfg(not(target_os = "linux"))]
fn notify_to(_addr: &std::ffi::OsStr) -> io::Result<()> {
    Ok(())
}

#[cfg(all(test, target_os = "linux"))]
mod notify_tests {
    use super::*;
    use std::os::linux::net::SocketAddrExt;
    use std::os::unix::net::{SocketAddr, UnixDatagram};
    use std::sync::Mutex;
    use std::time::Duration;

    /// `NOTIFY_SOCKET` is the process's: the tests take turns.
    static ENV: Mutex<()> = Mutex::new(());

    fn heard(sock: &UnixDatagram) -> Option<String> {
        sock.set_read_timeout(Some(Duration::from_millis(300))).unwrap();
        let mut buf = [0u8; 64];
        sock.recv(&mut buf).ok().map(|n| String::from_utf8_lossy(&buf[..n]).into_owned())
    }

    #[test]
    fn notify_ready_sends_ready_to_a_bound_datagram() {
        let _g = ENV.lock().unwrap_or_else(|e| e.into_inner());
        // a path
        let dir = std::env::temp_dir().join(format!("fantastic_notify_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("notify");
        let sock = UnixDatagram::bind(&path).unwrap();
        std::env::set_var("NOTIFY_SOCKET", &path);
        notify_ready();
        assert_eq!(heard(&sock).as_deref(), Some("READY=1"));
        // the abstract namespace
        let name = format!("fantastic-notify-{}", std::process::id());
        let abs = UnixDatagram::bind_addr(&SocketAddr::from_abstract_name(name.as_bytes()).unwrap()).unwrap();
        std::env::set_var("NOTIFY_SOCKET", format!("@{name}"));
        notify_ready();
        assert_eq!(heard(&abs).as_deref(), Some("READY=1"));
        // unset sends nothing
        std::env::remove_var("NOTIFY_SOCKET");
        notify_ready();
        assert_eq!(heard(&sock), None);
        assert_eq!(heard(&abs), None);
        // a socket nobody listens on is an error, said, not a panic
        assert!(notify_to(std::ffi::OsStr::new("/nonexistent/fantastic/notify")).is_err());
        assert!(notify_to(std::ffi::OsStr::new("relative")).is_err());
        std::fs::remove_dir_all(&dir).ok();
    }
}
