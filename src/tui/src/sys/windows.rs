//! The Windows side of [`crate::sys`]: std handles, the console,
//! console control events, the user's shell.

use std::fs::{File, OpenOptions};
use std::io;
use std::os::windows::io::{AsHandle, FromRawHandle, IntoRawHandle};

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
    use windows_sys::Win32::System::Console::{AttachConsole, GenerateConsoleCtrlEvent, CTRL_BREAK_EVENT};
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
    eprintln!("fantastic: ctrl-break {pid}: {step}: {}", io::Error::last_os_error());
    std::process::exit(1)
}

/// None: a Windows console (a ConPTY too) has no foreground process group — no
/// job control — so there is no row that says which command a shell launched,
/// and the last-command watch (`kern/term_watch`) sees none.
pub(crate) fn process_table() -> Option<String> {
    None
}

/// systemd's readiness message: there is no systemd here.
pub(crate) fn notify_ready() {}
