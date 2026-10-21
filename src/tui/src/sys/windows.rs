//! The Windows side of [`crate::sys`]: std handles, the console, local sockets,
//! console control events, the user's shell.
//!
//! THE LOCAL SOCKET is a loopback TCP connection found through a RENDEZVOUS
//! FILE at the socket's path. Not AF_UNIX (tokio has none on Windows), not a
//! named pipe: a pipe has no half-close, and `fantastic mcp`'s relay ends a
//! session by shutting its write half and then reading what is still in flight
//! (`mcp::attach`). The file plays the unix socket file's part — it exists
//! while the socket is served (written whole, once the listener is bound),
//! removing it is closing, and a client finds the port in it. It is PRIVATE
//! from its first byte (`fantastic-file`'s `0o600` create), because it also
//! holds a TOKEN a connection must open with: any local user can reach a
//! loopback port, only this user can read the file. A connection that does not
//! open with the token is dropped.

use std::fs::{File, OpenOptions};
use std::io;
use std::net::{Ipv4Addr, SocketAddr};
use std::os::windows::io::{AsHandle, FromRawHandle, IntoRawHandle};
use std::path::Path;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use windows_sys::Win32::Foundation::HANDLE;
use windows_sys::Win32::Globalization::CP_UTF8;
use windows_sys::Win32::System::Console::{
    SetConsoleOutputCP, SetStdHandle, STD_ERROR_HANDLE, STD_OUTPUT_HANDLE,
};

/// The console's active screen buffer, which the TUI draws on.
pub(crate) const TTY: &str = "CONOUT$";
/// The sink that discards everything written to it.
pub(crate) const NULL_DEVICE: &str = "NUL";
/// The variable naming the user's shell — what the terminal runs: `cmd.exe`,
/// the one Windows always names, as unix names `$SHELL`.
pub(crate) const SHELL_VAR: &str = "COMSPEC";

/// The console, opened read-write to draw the TUI on: its active screen buffer,
/// whatever stdout is (crossterm reads keys from `CONIN$`, and sets raw mode
/// there, on its own). Its output code page becomes UTF-8: the TUI writes UTF-8
/// bytes (box drawing, the title's blocks) and a console reads a byte write in
/// its code page — std's stdout converts to UTF-16 instead, but stdout is the
/// kernel log here. It stays UTF-8: what is said after the screen is restored
/// is UTF-8 too.
pub(crate) fn open_tty() -> io::Result<File> {
    let tty = OpenOptions::new().read(true).write(true).open(TTY)?;
    // SAFETY: no pointers; it sets this console's output code page.
    if unsafe { SetConsoleOutputCP(CP_UTF8) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(tty)
}

/// Point stdout and stderr at `file`: the std handles every `print!` writes
/// through (std looks them up on each write) and every child spawned with
/// inherited stdio gets. Each is a handle of its own, kept for the life of the
/// process, so the File may drop afterwards — dup2's independent duplicates.
pub(crate) fn redirect_stdio_to(file: &File) -> io::Result<()> {
    for which in [STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
        let h = file.try_clone()?.into_raw_handle();
        // SAFETY: `h` is a handle this process owns; once installed it is never
        // closed.
        if unsafe { SetStdHandle(which, h as HANDLE) } == 0 {
            let e = io::Error::last_os_error();
            // SAFETY: not installed, so still ours alone: closed here.
            drop(unsafe { File::from_raw_handle(h) });
            return Err(e);
        }
    }
    Ok(())
}

/// A File of its own on what stdout is now — kept before stdout is redirected.
pub(crate) fn dup_stdout() -> io::Result<File> {
    Ok(File::from(io::stdout().as_handle().try_clone_to_owned()?))
}

/// A File of its own on what stderr is now — kept before stderr is redirected.
pub(crate) fn dup_stderr() -> io::Result<File> {
    Ok(File::from(io::stderr().as_handle().try_clone_to_owned()?))
}

/// A local socket: a connected stream, its listener and its two halves.
pub(crate) type LocalStream = tokio::net::TcpStream;
pub(crate) type LocalReadHalf = tokio::net::tcp::OwnedReadHalf;
pub(crate) type LocalWriteHalf = tokio::net::tcp::OwnedWriteHalf;

/// The listener: a loopback port, and the token its connections open with.
pub(crate) struct LocalListener {
    tcp: tokio::net::TcpListener,
    token: String,
}

/// How long a new connection has to send the token before it is dropped — so a
/// stranger that connects and says nothing holds up the next client no longer.
const TOKEN_WAIT: Duration = Duration::from_secs(2);

impl LocalListener {
    /// The next connection that opened with the token (and its peer); any other
    /// is dropped unanswered.
    pub(crate) async fn accept(&self) -> io::Result<(LocalStream, SocketAddr)> {
        loop {
            let (mut s, peer) = self.tcp.accept().await?;
            let mut got = vec![0u8; self.token.len()];
            let sent = tokio::time::timeout(TOKEN_WAIT, s.read_exact(&mut got)).await;
            // small frames, as over a unix socket: no Nagle delay
            if matches!(sent, Ok(Ok(_)))
                && same(&got, self.token.as_bytes())
                && s.set_nodelay(true).is_ok()
            {
                return Ok((s, peer));
            }
        }
    }
}

/// Equal, in a time that does not tell where they differ.
fn same(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |d, (x, y)| d | (x ^ y)) == 0
}

/// Connect to the local socket at `path`: the port its rendezvous file names,
/// opened with the file's token.
pub(crate) async fn connect_local(path: &str) -> io::Result<LocalStream> {
    let text = std::fs::read_to_string(path)?;
    let (port, token) = text
        .trim()
        .split_once(' ')
        .and_then(|(p, t)| Some((p.parse::<u16>().ok()?, t)))
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("{path} names no port and token"),
            )
        })?;
    let mut s = LocalStream::connect((Ipv4Addr::LOCALHOST, port)).await?;
    s.set_nodelay(true)?;
    s.write_all(token.as_bytes()).await?;
    Ok(s)
}

/// Serve the local socket at `path` (relative to the process cwd): listen on a
/// free loopback port, then write the rendezvous file `<port> <token>` — private
/// before its first byte and whole or not at all (`fantastic-file`'s atomic
/// `0o600` write).
pub(crate) fn bind_local(path: &str) -> io::Result<LocalListener> {
    let l = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
    l.set_nonblocking(true)?;
    let tcp = tokio::net::TcpListener::from_std(l)?;
    let token = fantastic_kernel::lifecycle::token_hex(16);
    let line = format!("{} {token}\n", tcp.local_addr()?.port());
    fantastic_file::fs::write_bytes(Path::new("."), path, line.as_bytes(), true, Some(0o600))
        .map_err(io::Error::other)?;
    Ok(LocalListener { tcp, token })
}

/// The rendezvous file at `path` is created private by [`bind_local`] (it holds
/// the token): nothing is left to narrow.
pub(crate) fn make_private(_path: &str) -> io::Result<()> {
    Ok(())
}

/// What [`terminate`] answers: each Ctrl-Break, from `recv`.
pub(crate) type Terminate = tokio::signal::windows::CtrlBreak;
/// What [`interrupt`] answers: each Ctrl-C, from `recv`.
pub(crate) type Interrupt = tokio::signal::windows::CtrlC;

/// The termination request: Ctrl-Break, what a polite stop sends a process
/// group on Windows (the e2e harness's `stop`) — its SIGTERM.
pub(crate) fn terminate() -> io::Result<Terminate> {
    tokio::signal::windows::ctrl_break()
}

/// The interrupt (Ctrl-C).
pub(crate) fn interrupt() -> io::Result<Interrupt> {
    tokio::signal::windows::ctrl_c()
}

/// `fantastic __ctrl_break <pid>` — THE STOP HELPER. A host with no console
/// (the desktop app) cannot reach a canvas's process group:
/// `GenerateConsoleCtrlEvent` reaches a group only through a console the caller
/// shares. This helper is started with no console of its own, attaches to the
/// canvas's (`AttachConsole(pid)`) and sends it Ctrl-Break, every fantastic's
/// SIGTERM. Exit 0 sent; 1 a step failed (named on stderr); 2 a bad argument.
pub(crate) fn ctrl_break_main(argv: &[String]) -> ! {
    use windows_sys::Win32::System::Console::{
        AttachConsole, GenerateConsoleCtrlEvent, CTRL_BREAK_EVENT,
    };
    let pid: u32 = match argv {
        [one] => match one.parse() {
            Ok(p) => p,
            Err(_) => usage(),
        },
        _ => usage(),
    };
    // SAFETY: no pointers; it attaches this process to `pid`'s console.
    if unsafe { AttachConsole(pid) } == 0 {
        failed(pid, "AttachConsole");
    }
    // SAFETY: no pointers; it signals the process group `pid` leads on the
    // console now shared.
    if unsafe { GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT, pid) } == 0 {
        failed(pid, "GenerateConsoleCtrlEvent");
    }
    std::process::exit(0)
}

fn usage() -> ! {
    eprintln!("fantastic: ctrl-break: usage: fantastic __ctrl_break <pid>");
    std::process::exit(2)
}

fn failed(pid: u32, step: &str) -> ! {
    eprintln!(
        "fantastic: ctrl-break {pid}: {step}: {}",
        io::Error::last_os_error()
    );
    std::process::exit(1)
}

/// None: a Windows console (a ConPTY too) has no foreground process group — no
/// job control — so there is no row that says which command a shell launched,
/// and the last-command watch (`kern/term_watch`) sees none.
pub(crate) fn process_table() -> Option<String> {
    None
}
