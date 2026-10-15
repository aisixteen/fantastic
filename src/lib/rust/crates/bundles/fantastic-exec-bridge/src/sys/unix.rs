//! Unix implementation of the exec_bridge OS boundary.

use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

pub(crate) use nix::sys::signal::Signal;
pub(crate) use std::os::fd::OwnedFd;
/// The std control-socket type (one end of [`socket_pair`]).
pub(crate) use std::os::unix::net::UnixStream as StdUnixStream;
pub(crate) use tokio::io::unix::AsyncFd;
pub(crate) use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
pub(crate) use tokio::net::UnixStream;

/// The control socket pair: `(parent, child)` ends.
///
/// ## Why `std::os::unix::net::UnixStream::pair` and not `nix::socketpair`
///
/// `nix` 0.29 gates `SockFlag::SOCK_CLOEXEC` to linux/BSD/solaris — it does not exist
/// on macOS, so `nix` cannot produce a close-on-exec pair here without a `#[cfg]` fork.
/// std sets `FD_CLOEXEC` on both ends on every unix, for free.
///
/// And CLOEXEC-by-default is load-bearing, not hygiene. This process forks from several
/// places concurrently (other jobs, PTY jobs). A
/// non-CLOEXEC child end would leak a copy into every one of those, and each leaked copy
/// holds the socket open — so the connector's EOF self-reap (`_o._exit(0)`) would never
/// fire and parked tools would outlive the kernel. [`child_setup`] then clears the flag
/// on exactly the one fd the child is meant to keep.
pub(crate) fn socket_pair() -> std::io::Result<(StdUnixStream, StdUnixStream)> {
    StdUnixStream::pair()
}

/// The fd number of a control-socket end.
pub(crate) fn raw_fd(s: &StdUnixStream) -> i32 {
    s.as_raw_fd()
}

/// The error a PATH search that found nothing answers (`ENOENT`).
pub(crate) fn not_found() -> std::io::Error {
    std::io::Error::from_raw_os_error(libc::ENOENT)
}

/// Whether `full` is a regular file this process may execute (`access(X_OK)`).
pub(crate) fn is_executable(full: &Path) -> bool {
    let Ok(c) = std::ffi::CString::new(full.as_os_str().as_bytes()) else {
        return false;
    };
    // SAFETY: `access` on a NUL-terminated path we own.
    full.is_file() && unsafe { libc::access(c.as_ptr(), libc::X_OK) } == 0
}

/// Every pid the OS lists right now — `proc_listallpids` on macOS, `/proc` on
/// Linux (python `proc._all_pids`). Picked by target, statically.
pub(crate) fn all_pids() -> Vec<i32> {
    #[cfg(target_os = "macos")]
    {
        // SAFETY: a null buffer asks for the count; the second call fills a buffer
        // sized for it (plus slack for processes born in between).
        let n = unsafe { libc::proc_listallpids(std::ptr::null_mut(), 0) }.max(0) as usize;
        let mut buf = vec![0i32; n + 64];
        let bytes = (buf.len() * std::mem::size_of::<i32>()) as libc::c_int;
        let got = unsafe { libc::proc_listallpids(buf.as_mut_ptr().cast(), bytes) }.max(0);
        buf.truncate(got as usize);
        buf.retain(|p| *p > 0);
        buf
    }
    #[cfg(not(target_os = "macos"))]
    {
        std::fs::read_dir("/proc")
            .map(|rd| {
                rd.filter_map(|e| e.ok()?.file_name().to_str()?.parse::<i32>().ok())
                    .collect()
            })
            .unwrap_or_default()
    }
}

/// SIGKILL every member of `leader`'s session, then the leader. Returns whether
/// the leader was signalled.
pub(crate) fn kill_session(leader: i32) -> bool {
    for pid in all_pids() {
        // SAFETY: `getsid`/`kill` on a plain pid; a vanished one returns -1.
        if pid != leader && unsafe { libc::getsid(pid) } == leader {
            unsafe { libc::kill(pid, libc::SIGKILL) };
        }
    }
    // SAFETY: as above.
    unsafe { libc::kill(leader, libc::SIGKILL) == 0 }
}

/// Send `sig` to one pid. Returns whether it was delivered.
pub(crate) fn signal(pid: u32, sig: Signal) -> bool {
    nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid as i32), sig).is_ok()
}

/// python's `Process.returncode`: the exit status, or `-N` when a signal killed it.
pub(crate) fn exit_code(s: std::process::ExitStatus) -> Option<i32> {
    use std::os::unix::process::ExitStatusExt;
    s.code().or_else(|| s.signal().map(|n| -n))
}

/// A fresh pseudo-terminal `(master, slave)` sized `cols`×`rows`; the master is
/// close-on-exec.
pub(crate) fn open_pty(cols: u16, rows: u16) -> std::io::Result<(OwnedFd, OwnedFd)> {
    let mut master: libc::c_int = -1;
    let mut slave: libc::c_int = -1;
    let mut ws = libc::winsize {
        ws_row: rows,
        ws_col: cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    // SAFETY: valid out-pointers; a null name/termios is allowed.
    let rc = unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut ws,
        )
    };
    if rc != 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: openpty handed us two fresh fds we now own.
    let (master, slave) = unsafe { (OwnedFd::from_raw_fd(master), OwnedFd::from_raw_fd(slave)) };
    // The master stays with the kernel only: CLOEXEC so no other spawn inherits it.
    // SAFETY: fcntl on an fd we own.
    unsafe { libc::fcntl(master.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) };
    Ok((master, slave))
}

/// Set the window size on a PTY master.
pub(crate) fn set_winsize(fd: i32, cols: u16, rows: u16) -> std::io::Result<()> {
    let ws = libc::winsize {
        ws_row: rows,
        ws_col: cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    // SAFETY: a valid winsize on an fd we own.
    if unsafe { libc::ioctl(fd, libc::TIOCSWINSZ, &ws) } == -1 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// The raw fd behind a PTY master.
pub(crate) fn master_fd(fd: &OwnedFd) -> i32 {
    fd.as_raw_fd()
}

/// Put an fd in non-blocking mode (`O_NONBLOCK`).
pub(crate) fn set_nonblocking(fd: &OwnedFd) -> std::io::Result<()> {
    // SAFETY: fcntl on an fd we own.
    let flags = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFL) };
    if flags == -1
        || unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) } == -1
    {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// One `read(2)` on `fd` into `buf`.
pub(crate) fn read_fd(fd: &OwnedFd, buf: &mut [u8]) -> std::io::Result<usize> {
    // SAFETY: a valid buffer on an fd we own.
    let n = unsafe { libc::read(fd.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len()) };
    if n < 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(usize::try_from(n).unwrap_or(0))
    }
}

/// One `write(2)` of `data` on `fd`; may write short.
pub(crate) fn write_fd(fd: &OwnedFd, data: &[u8]) -> std::io::Result<usize> {
    // SAFETY: a valid buffer on an fd we own.
    let n = unsafe { libc::write(fd.as_raw_fd(), data.as_ptr().cast(), data.len()) };
    if n < 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(usize::try_from(n).unwrap_or(0))
    }
}

/// argv[0] as the caller wrote it, apart from the file exec'd.
pub(crate) fn set_arg0(cmd: &mut tokio::process::Command, arg0: &str) {
    cmd.arg0(arg0);
}

/// The child's post-fork setup: keep the control fd `ctrl_fd` across exec, lead a
/// new session, and for a PTY job take the slave on fd 0 as controlling terminal.
pub(crate) fn child_setup(cmd: &mut tokio::process::Command, ctrl_fd: i32, is_pty: bool) {
    // SAFETY: post-fork, pre-exec, in a multithreaded process — the closure must be
    // async-signal-safe. `fcntl` is; anything that allocates, logs, or takes a lock is
    // not. It captures the raw i32 ONLY: capturing the owning `UnixStream` would keep
    // the parent's copy of the child end alive for the Command's lifetime, and that copy
    // is exactly what must die (the caller drops it after spawning).
    unsafe {
        cmd.pre_exec(move || {
            // Clear FD_CLOEXEC so the control socket survives execve at the SAME number
            // the parent advertised in FANTASTIC_CTRL_FD.
            if libc::fcntl(ctrl_fd, libc::F_SETFD, 0) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            // EVERY job leads its own session, so `kill_session` takes the
            // command and everything it launched (`server &`, a `nohup`), and a
            // Ctrl-C at the kernel's terminal is the kernel's to handle.
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            // A PTY job's session has the slave (already on fd 0) as its
            // controlling terminal, so job control, Ctrl-C and SIGWINCH reach it.
            if is_pty && libc::ioctl(0, libc::TIOCSCTTY as _, 0) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

/// python `tempfile.mkdtemp(prefix=…)` — a 0700 dir in the system temp dir.
pub(crate) fn mkdtemp(prefix: &str) -> std::io::Result<PathBuf> {
    let template = std::env::temp_dir().join(format!("{prefix}XXXXXXXX"));
    let mut bytes = template.to_string_lossy().into_owned().into_bytes();
    bytes.push(0);
    // SAFETY: a NUL-terminated, writable template buffer.
    let ptr = unsafe { libc::mkdtemp(bytes.as_mut_ptr().cast()) };
    if ptr.is_null() {
        return Err(std::io::Error::last_os_error());
    }
    bytes.pop();
    Ok(PathBuf::from(String::from_utf8_lossy(&bytes).to_string()))
}
