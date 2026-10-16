//! Windows implementation of the exec_bridge OS boundary.
//!
//! - A job is started by `CreateProcessW` SUSPENDED, put into its OWN Job Object,
//!   then resumed — so everything it launches is born inside that job, and "kill
//!   the job" is `TerminateJobObject`: the Job Object is Windows' session. What the
//!   job inherits is named exactly: `STARTUPINFOEXW` with
//!   `PROC_THREAD_ATTRIBUTE_HANDLE_LIST` lists the control socket and the job's
//!   stdio, and no other handle of this process crosses.
//! - The control socket is a loopback TCP pair (Windows has no `socketpair`); the
//!   connector's `socket.socket(fileno=…)` takes the inherited SOCKET.
//! - A PTY job runs on a ConPTY pseudoconsole, and the kernel's end of it is two
//!   pipes.
//! - Anonymous pipes are synchronous — nothing tokio can poll — so each read end is
//!   drained by its own thread into a BOUNDED channel the async side reads.

use std::collections::{BTreeMap, HashMap};
use std::ffi::{c_void, OsStr};
use std::io;
use std::net::{Ipv4Addr, TcpListener, TcpStream};
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::{AsRawHandle, AsRawSocket, FromRawHandle, OwnedHandle};
use std::os::windows::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::process::ExitStatus;
use std::ptr::{null, null_mut};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};
use std::task::{Context, Poll};

use tokio::io::{AsyncRead, AsyncReadExt, ReadBuf};
use tokio::sync::{mpsc, oneshot};
use windows_sys::core::PWSTR;
use windows_sys::Win32::Foundation::{
    DuplicateHandle, LocalFree, SetHandleInformation, DUPLICATE_SAME_ACCESS, ERROR_ALREADY_EXISTS,
    ERROR_BROKEN_PIPE, ERROR_DIRECTORY, ERROR_FILE_NOT_FOUND, HANDLE, HANDLE_FLAG_INHERIT,
    INVALID_HANDLE_VALUE, WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows_sys::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows_sys::Win32::Security::{
    GetTokenInformation, TokenUser, PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES, TOKEN_QUERY,
    TOKEN_USER,
};
use windows_sys::Win32::Storage::FileSystem::{CreateDirectoryW, ReadFile, WriteFile};
use windows_sys::Win32::System::Console::{
    ClosePseudoConsole, CreatePseudoConsole, GenerateConsoleCtrlEvent, GetStdHandle,
    ResizePseudoConsole, COORD, CTRL_BREAK_EVENT, HPCON, STD_INPUT_HANDLE,
};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, TerminateJobObject,
};
use windows_sys::Win32::System::Pipes::CreatePipe;
use windows_sys::Win32::System::Threading::{
    CreateProcessW, DeleteProcThreadAttributeList, GetCurrentProcess, GetExitCodeProcess,
    InitializeProcThreadAttributeList, OpenProcessToken, ResumeThread, TerminateProcess,
    UpdateProcThreadAttribute, WaitForSingleObject, CREATE_NEW_PROCESS_GROUP, CREATE_SUSPENDED,
    CREATE_UNICODE_ENVIRONMENT, EXTENDED_STARTUPINFO_PRESENT, INFINITE,
    LPPROC_THREAD_ATTRIBUTE_LIST, PROCESS_INFORMATION, PROC_THREAD_ATTRIBUTE_HANDLE_LIST,
    PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE, STARTF_USESTDHANDLES, STARTUPINFOEXW,
};

// ── the control socket ──────────────────────────────────────────────

/// One end of the control socket pair, before it is handed to tokio (the
/// relay's) or to a job (the child's).
pub(crate) type CtrlEnd = TcpStream;
/// The relay's end of the control socket.
pub type CtrlStream = tokio::net::TcpStream;
/// The read half of a [`CtrlStream`].
pub type CtrlReadHalf = tokio::net::tcp::OwnedReadHalf;
/// The write half of a [`CtrlStream`].
pub type CtrlWriteHalf = tokio::net::tcp::OwnedWriteHalf;

/// The control socket pair: `(parent, child)` ends — a CONNECTED loopback TCP
/// pair, since Windows has no `socketpair`.
///
/// Listen on 127.0.0.1:0, connect, accept — and keep the accepted end only when
/// its peer is exactly our connecting socket. Any local process may connect to a
/// listening loopback port; one that raced in ahead of ours would otherwise be
/// handed a job's control channel. Such a connection is dropped and the accept
/// repeats: ours is already queued (`connect` returned), so it comes. The
/// listener closes the moment the pair exists.
///
/// std creates every socket NOT inheritable (`WSA_FLAG_NO_HANDLE_INHERIT`), the
/// close-on-exec of this OS: [`spawn`] lets exactly one job inherit the child
/// end, through its handle list.
///
/// `TCP_NODELAY` on both ends: the wire is small frames, and Nagle plus a delayed
/// ACK would hold a frame written behind an unacknowledged one for up to 200 ms.
pub(crate) fn ctrl_pair() -> io::Result<(CtrlEnd, CtrlEnd)> {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
    let child = TcpStream::connect(listener.local_addr()?)?;
    let ours = child.local_addr()?;
    let parent = loop {
        let (s, peer) = listener.accept()?;
        if peer == ours {
            break s;
        }
    };
    drop(listener);
    parent.set_nodelay(true)?;
    child.set_nodelay(true)?;
    Ok((parent, child))
}

/// The number `FANTASTIC_CTRL_FD` advertises for a control-socket end: its
/// SOCKET handle, which the job inherits at the same value.
pub(crate) fn ctrl_number(end: &CtrlEnd) -> u64 {
    end.as_raw_socket()
}

/// The relay's end, made async (non-blocking, registered with tokio).
pub(crate) fn ctrl_stream(end: CtrlEnd) -> io::Result<CtrlStream> {
    end.set_nonblocking(true)?;
    CtrlStream::from_std(end)
}

// ── what every child's environment holds (python `OS_ENV`) ────────────

/// The variables the OS needs in every process it starts, passed down whatever
/// the record's `inherit_env` says: `SYSTEMROOT` — without it a child cannot
/// start Winsock, so it could not open the control socket (or any socket) at all.
pub(crate) const OS_ENV: &[&str] = &["SYSTEMROOT"];

// ── the child's own shell (python `SHELL_VAR` / `LOGIN_ARGS`) ─────────

/// The child env var that names its shell: `COMSPEC`, the command interpreter
/// every Windows environment names (`SHELL` is a unix convention, unset here).
pub(crate) const SHELL_VAR: &str = "COMSPEC";

/// What makes that shell a LOGIN shell: nothing — `cmd.exe` has no login mode
/// (a console session is already the user's), so `shell: "login"` permits the
/// same line as `shell: true`.
pub(crate) const LOGIN_ARGS: &[&str] = &[];

// ── the PATH grammar (python `proc.resolve`, `shutil.which` on Windows) ──

/// The separator between `PATH` entries (`os.pathsep`).
pub(crate) const PATH_LIST_SEP: char = ';';

/// python `shutil._WIN_DEFAULT_PATHEXT`: what `which` tries when `PATHEXT` is unset.
const DEFAULT_PATHEXT: &str = ".COM;.EXE;.BAT;.CMD;.VBS;.JS;.WS;.MSC";

/// A command naming a file — run as written, never searched: one with a `/` or `\`.
pub(crate) fn names_a_file(command: &str) -> bool {
    command.contains(['/', '\\'])
}

/// The file names a bare command may be found under in a `PATH` dir — python
/// 3.11 `shutil.which`: the name as written when it already ends in a `PATHEXT`
/// extension, else the name with each extension appended (`python` is
/// `python.COM`, `python.EXE`, …). `PATHEXT` is read from the kernel's own
/// environment, as `which` reads `os.environ`: which extensions are executable is
/// the OS's judgement, as `access(X_OK)` is on unix — not the child's `PATH`.
pub(crate) fn exe_names(command: &str) -> Vec<String> {
    let source = std::env::var("PATHEXT")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| DEFAULT_PATHEXT.to_string());
    let exts: Vec<&str> = source
        .split(PATH_LIST_SEP)
        .filter(|e| !e.is_empty())
        .collect();
    let lower = command.to_lowercase();
    if exts.iter().any(|e| lower.ends_with(&e.to_lowercase())) {
        return vec![command.to_string()];
    }
    exts.iter().map(|e| format!("{command}{e}")).collect()
}

/// The error a PATH search that found nothing answers (`ERROR_FILE_NOT_FOUND`,
/// python's `ENOENT`).
pub(crate) fn not_found() -> io::Error {
    io::Error::from_raw_os_error(ERROR_FILE_NOT_FOUND as i32)
}

/// Whether `full` is a file this process may run — any regular file: Windows has
/// no execute bit (python's `os.access(X_OK)` is existence there).
pub(crate) fn is_executable(full: &Path) -> bool {
    full.is_file()
}

// ── jobs ────────────────────────────────────────────────────────────

/// The exit code a job [`kill_session`] killed reports: what `TerminateJobObject`
/// gives every process in it — 1, python's `Popen.kill()` (`TerminateProcess(h,
/// 1)`). Windows has no signal numbers to negate.
pub(crate) const KILLED_EXIT_CODE: i32 = 1;

/// A spawned job — the part of tokio's `Child` this crate (and `py_agent`) uses,
/// over the job's process handle.
pub struct Child {
    /// A pipe job's stdout; `None` for a PTY job, whose output is the terminal's.
    pub stdout: Option<PipeReader>,
    /// A pipe job's stderr; `None` for a PTY job.
    pub stderr: Option<PipeReader>,
    pid: u32,
    process: Arc<OwnedHandle>,
    /// Resolved by the exit watcher (see [`watch_exit`]); `None` once seen.
    exited: Option<oneshot::Receiver<io::Result<()>>>,
}

impl Child {
    /// The job's process id — the key [`kill_session`] finds its Job Object by.
    pub fn id(&self) -> Option<u32> {
        Some(self.pid)
    }

    /// Wait for the process to exit, and answer its status. Cancel-safe and
    /// repeatable, like tokio's.
    pub async fn wait(&mut self) -> io::Result<ExitStatus> {
        if let Some(rx) = self.exited.as_mut() {
            let watched = rx.await;
            self.exited = None;
            watched.map_err(|_| io::Error::other("exec_bridge: the job's exit watcher died"))??;
        }
        status_now(&self.process)?
            .ok_or_else(|| io::Error::other("exec_bridge: the job has not exited"))
    }

    /// The exit status if the process has exited, without waiting.
    pub fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        status_now(&self.process)
    }
}

impl Drop for Child {
    fn drop(&mut self) {
        // The pid names this job only while its Child lives: past it, nothing holds
        // the process handle that keeps Windows from reusing the number.
        job_table().remove(&self.pid);
    }
}

/// The process's exit status, if it has exited.
fn status_now(process: &OwnedHandle) -> io::Result<Option<ExitStatus>> {
    // SAFETY: a zero-timeout wait on a process handle we own.
    match unsafe { WaitForSingleObject(process.as_raw_handle(), 0) } {
        WAIT_OBJECT_0 => {
            let mut code = 0u32;
            // SAFETY: as above; `code` outlives the call.
            if unsafe { GetExitCodeProcess(process.as_raw_handle(), &mut code) } == 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(Some(ExitStatus::from_raw(code)))
        }
        WAIT_TIMEOUT => Ok(None),
        _ => Err(io::Error::last_os_error()),
    }
}

/// Every live job's Job Object, by its process id — what [`kill_session`]
/// terminates. An entry lives as long as the job's [`Child`]; the process handle
/// the Child holds keeps Windows from reusing the pid meanwhile.
fn job_table() -> MutexGuard<'static, HashMap<u32, OwnedHandle>> {
    static JOBS: OnceLock<Mutex<HashMap<u32, OwnedHandle>>> = OnceLock::new();
    JOBS.get_or_init(Mutex::default)
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
}

/// Start `program` (argv[0] `arg0`, then `args`) in `cwd` with EXACTLY `env`
/// (later pairs win), letting it inherit the control socket end `ctrl` — and
/// nothing else of this process — inside its own Job Object. A pipe job's
/// stdout/stderr are pipes (stdin is the kernel's, as python's
/// `create_subprocess_exec` leaves it) and it leads its own process group; a PTY
/// job runs on the slave's pseudoconsole.
pub(crate) fn spawn(
    program: &Path,
    arg0: &str,
    args: &[String],
    env: &[(String, String)],
    cwd: &str,
    ctrl: &CtrlEnd,
    pty: Option<PtySlave>,
) -> io::Result<Child> {
    let app = wide(program.as_os_str())?;
    let argv: Vec<&str> = std::iter::once(arg0)
        .chain(args.iter().map(String::as_str))
        .collect();
    let mut cmdline = wide(OsStr::new(&list2cmdline(&argv)))?;
    let block = env_block(env)?;
    let dir = wide(OsStr::new(cwd))?;
    // The job's container exists before the job does.
    let job = new_job()?;

    // SAFETY: zeroed is a valid STARTUPINFOEXW (null pointers, no flags).
    let mut si: STARTUPINFOEXW = unsafe { std::mem::zeroed() };
    si.StartupInfo.cb = std::mem::size_of::<STARTUPINFOEXW>() as u32;
    // The std handles are ALWAYS the ones named here. Unset, a PTY job would pick
    // up the kernel's own (a redirected kernel's log file) instead of its console;
    // left null, a console job attaching to its pseudoconsole gets the console's.
    si.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
    let mut flags = CREATE_SUSPENDED | CREATE_UNICODE_ENVIRONMENT | EXTENDED_STARTUPINFO_PRESENT;
    // The parent's copies of what the child inherits; closed once it has its own,
    // or a pipe never reaches EOF.
    let mut child_ends: Vec<OwnedHandle> = Vec::new();
    let mut read_ends = None;
    let console = match pty {
        None => {
            // Its own process group: what a pipe job's `interrupt` (CTRL_BREAK)
            // addresses — and a Ctrl-C at the kernel's console does not reach it.
            flags |= CREATE_NEW_PROCESS_GROUP;
            let (out_r, out_w) = pipe()?;
            let (err_r, err_w) = pipe()?;
            if let Some(stdin) = inherited_stdin()? {
                si.StartupInfo.hStdInput = stdin.as_raw_handle();
                child_ends.push(stdin);
            }
            si.StartupInfo.hStdOutput = out_w.as_raw_handle();
            si.StartupInfo.hStdError = err_w.as_raw_handle();
            child_ends.extend([out_w, err_w]);
            read_ends = Some((out_r, err_r));
            None
        }
        Some(PtySlave(console)) => Some(console),
    };
    // EXACTLY these cross into the job. Each must be inheritable for the call; the
    // list keeps them from leaking into any other process this one starts.
    let ctrl_handle = ctrl.as_raw_socket() as usize as HANDLE;
    let mut inherit = vec![ctrl_handle];
    inherit.extend(child_ends.iter().map(|h| h.as_raw_handle()));
    for h in &inherit {
        set_inherit(*h, true)?;
    }
    let mut attrs = AttrList::new(if console.is_some() { 2 } else { 1 })?;
    // SAFETY (both `set`s): `inherit` and the pseudoconsole outlive the
    // CreateProcessW below, and `inherit` is not touched again before it.
    attrs.set(
        PROC_THREAD_ATTRIBUTE_HANDLE_LIST,
        inherit.as_ptr().cast(),
        inherit.len() * std::mem::size_of::<HANDLE>(),
    )?;
    if let Some(c) = &console {
        // The attribute's value IS the HPCON, not a pointer to one.
        attrs.set(
            PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE,
            c.handle()? as *const c_void,
            std::mem::size_of::<HPCON>(),
        )?;
    }
    si.lpAttributeList = attrs.as_ptr();

    // SAFETY: zeroed is a valid PROCESS_INFORMATION (an out-parameter).
    let mut pi: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };
    // SAFETY: every string is NUL-terminated and owned above, the command line
    // is writable (CreateProcessW may modify it), the environment block is
    // double-NUL-terminated UTF-16 (CREATE_UNICODE_ENVIRONMENT), and `si` with
    // its attribute list outlives the call.
    let created = unsafe {
        CreateProcessW(
            app.as_ptr(),
            cmdline.as_mut_ptr(),
            null(),
            null(),
            1,
            flags,
            block.as_ptr().cast(),
            dir.as_ptr(),
            &si.StartupInfo,
            &mut pi,
        )
    } != 0;
    let created = if created {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    };
    // The control end is inheritable for that one call only; the caller drops it
    // next. (A spawn elsewhere in this process WITHOUT a handle list — std's — could
    // still inherit it in that window; every spawn here names its list.)
    let _ = set_inherit(ctrl_handle, false);
    drop(attrs);
    drop(child_ends);
    created?;
    // SAFETY: CreateProcessW handed us both handles; we own them now.
    let (process, thread) = unsafe {
        (
            OwnedHandle::from_raw_handle(pi.hProcess),
            OwnedHandle::from_raw_handle(pi.hThread),
        )
    };

    // Into its own job BEFORE it runs one instruction: everything it launches is
    // born inside, and a job's processes cannot leave it.
    // SAFETY: a job and a suspended process we own.
    if unsafe { AssignProcessToJobObject(job.as_raw_handle(), process.as_raw_handle()) } == 0 {
        let e = io::Error::last_os_error();
        // SAFETY: as above; it never ran.
        unsafe { TerminateProcess(process.as_raw_handle(), KILLED_EXIT_CODE as u32) };
        return Err(e);
    }
    let abort = |e: io::Error| {
        // SAFETY: a job we own; the process in it never ran.
        unsafe { TerminateJobObject(job.as_raw_handle(), KILLED_EXIT_CODE as u32) };
        e
    };
    let (stdout, stderr) = match read_ends {
        None => (None, None),
        Some((out_r, err_r)) => (
            Some(PipeReader::spawn(out_r).map_err(abort)?),
            Some(PipeReader::spawn(err_r).map_err(abort)?),
        ),
    };
    let process = Arc::new(process);
    let exited = watch_exit(Arc::clone(&process), console).map_err(abort)?;
    // SAFETY: the job's main thread, created suspended.
    if unsafe { ResumeThread(thread.as_raw_handle()) } == u32::MAX {
        return Err(abort(io::Error::last_os_error()));
    }
    job_table().insert(pi.dwProcessId, job);
    Ok(Child {
        stdout,
        stderr,
        pid: pi.dwProcessId,
        process,
        exited: Some(exited),
    })
}

/// The process's exit, as a future. A plain thread blocks on the process handle —
/// not `spawn_blocking`: a job may run for the kernel's whole life, and a tokio
/// runtime waits for its blocking tasks before it can shut down.
///
/// For a PTY job it then CLOSES the pseudoconsole. ConPTY keeps the terminal's
/// output pipe open for as long as the console lives, so without this a PTY job
/// would never reach EOF and its pump would never end; this is what the slave's
/// last close is on unix (it also ends whatever is still attached to the
/// console).
fn watch_exit(
    process: Arc<OwnedHandle>,
    console: Option<Arc<PseudoConsole>>,
) -> io::Result<oneshot::Receiver<io::Result<()>>> {
    let (tx, rx) = oneshot::channel();
    std::thread::Builder::new()
        .name("exec_bridge job".to_string())
        .spawn(move || {
            // SAFETY: a process handle this thread co-owns.
            let r = match unsafe { WaitForSingleObject(process.as_raw_handle(), INFINITE) } {
                WAIT_OBJECT_0 => Ok(()),
                _ => Err(io::Error::last_os_error()),
            };
            let _ = tx.send(r);
            if let Some(c) = console {
                c.close();
            }
        })?;
    Ok(rx)
}

/// A fresh Job Object, unnamed, no limits.
fn new_job() -> io::Result<OwnedHandle> {
    // SAFETY: default security, no name.
    let h = unsafe { CreateJobObjectW(null(), null()) };
    if h.is_null() {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: a fresh handle we now own.
    Ok(unsafe { OwnedHandle::from_raw_handle(h) })
}

/// The kernel's stdin, duplicated inheritable for a pipe job — what std's
/// `Stdio::inherit` does on Windows. `None` when the kernel has none.
fn inherited_stdin() -> io::Result<Option<OwnedHandle>> {
    // SAFETY: reads this process's std handle; no ownership taken.
    let h = unsafe { GetStdHandle(STD_INPUT_HANDLE) };
    if h.is_null() || h == INVALID_HANDLE_VALUE {
        return Ok(None);
    }
    let mut dup: HANDLE = null_mut();
    // SAFETY: duplicates a live handle within this process into `dup`.
    let ok = unsafe {
        DuplicateHandle(
            GetCurrentProcess(),
            h,
            GetCurrentProcess(),
            &mut dup,
            0,
            1,
            DUPLICATE_SAME_ACCESS,
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: a fresh handle we now own.
    Ok(Some(unsafe { OwnedHandle::from_raw_handle(dup) }))
}

/// Set or clear `HANDLE_FLAG_INHERIT` (a SOCKET is a handle here too).
fn set_inherit(h: HANDLE, on: bool) -> io::Result<()> {
    let flags = if on { HANDLE_FLAG_INHERIT } else { 0 };
    // SAFETY: a handle we own, open for the call.
    if unsafe { SetHandleInformation(h, HANDLE_FLAG_INHERIT, flags) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// A `PROC_THREAD_ATTRIBUTE_LIST` with room for a fixed number of attributes;
/// deleted on drop. The VALUES it points at must outlive the `CreateProcessW` that
/// reads them — the caller keeps them alive.
struct AttrList(Vec<usize>);

impl AttrList {
    fn new(count: u32) -> io::Result<Self> {
        let mut size = 0usize;
        // SAFETY: a null list asks for the size it needs (failing, by design).
        unsafe { InitializeProcThreadAttributeList(null_mut(), count, 0, &mut size) };
        let mut buf = vec![0usize; size.div_ceil(std::mem::size_of::<usize>())];
        // SAFETY: a zeroed buffer of at least `size` bytes, pointer-aligned.
        if unsafe {
            InitializeProcThreadAttributeList(buf.as_mut_ptr().cast(), count, 0, &mut size)
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(Self(buf))
    }

    fn as_ptr(&mut self) -> LPPROC_THREAD_ATTRIBUTE_LIST {
        self.0.as_mut_ptr().cast()
    }

    fn set(&mut self, attribute: u32, value: *const c_void, size: usize) -> io::Result<()> {
        // SAFETY: an initialized list; the caller keeps `value` alive.
        let ok = unsafe {
            UpdateProcThreadAttribute(
                self.as_ptr(),
                0,
                attribute as usize,
                value,
                size,
                null_mut(),
                null(),
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}

impl Drop for AttrList {
    fn drop(&mut self) {
        // SAFETY: initialized in `new`, deleted once.
        unsafe { DeleteProcThreadAttributeList(self.as_ptr()) };
    }
}

/// python `subprocess.list2cmdline`: argv as ONE command line, quoted the way the
/// MS C runtime splits it back — what `CreateProcessW` takes, and what python's
/// `create_subprocess_exec` builds on Windows.
fn list2cmdline(argv: &[&str]) -> String {
    let mut out = String::new();
    for arg in argv {
        if !out.is_empty() {
            out.push(' ');
        }
        let quote = arg.is_empty() || arg.contains([' ', '\t']);
        if quote {
            out.push('"');
        }
        // Backslashes are literal — unless a quote follows them, when each is
        // doubled and the quote escaped.
        let mut backslashes = 0usize;
        for c in arg.chars() {
            match c {
                '\\' => backslashes += 1,
                '"' => {
                    out.push_str(&"\\".repeat(backslashes * 2));
                    backslashes = 0;
                    out.push_str("\\\"");
                }
                _ => {
                    out.push_str(&"\\".repeat(backslashes));
                    backslashes = 0;
                    out.push(c);
                }
            }
        }
        out.push_str(&"\\".repeat(backslashes));
        if quote {
            // …and a closing quote follows the trailing ones.
            out.push_str(&"\\".repeat(backslashes));
            out.push('"');
        }
    }
    out
}

/// The environment block `CreateProcessW` takes: `NAME=value\0…\0`, UTF-16.
/// Windows names are case-insensitive, so a later pair REPLACES an earlier one
/// whatever its case (a later `env()` wins on unix too), and the block is sorted
/// by name, as `CreateProcessW` documents.
fn env_block(env: &[(String, String)]) -> io::Result<Vec<u16>> {
    let mut vars: BTreeMap<String, (&str, &str)> = BTreeMap::new();
    for (k, v) in env {
        // python `_winapi`: an empty name, or an `=` past the first character (a
        // leading one is a drive's cwd, `=C:`), names no variable.
        if k.is_empty() || k.chars().skip(1).any(|c| c == '=') {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "illegal environment variable name",
            ));
        }
        if k.contains('\0') || v.contains('\0') {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "embedded null character",
            ));
        }
        vars.insert(k.to_uppercase(), (k, v));
    }
    let mut block: Vec<u16> = Vec::new();
    for (k, v) in vars.values() {
        block.extend(k.encode_utf16());
        block.push(u16::from(b'='));
        block.extend(v.encode_utf16());
        block.push(0);
    }
    if block.is_empty() {
        block.push(0);
    }
    block.push(0);
    Ok(block)
}

/// `s` as a NUL-terminated UTF-16 string; an embedded NUL would cut it short, so
/// it is refused.
fn wide(s: &OsStr) -> io::Result<Vec<u16>> {
    let mut w: Vec<u16> = s.encode_wide().collect();
    if w.contains(&0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "embedded null character",
        ));
    }
    w.push(0);
    Ok(w)
}

/// Kill the job `leader` leads: `TerminateJobObject` on its Job Object — the
/// program and everything it launched, which cannot have left the job. Returns
/// whether the job was terminated (`false` for a pid that names no live job).
pub(crate) fn kill_session(leader: i32) -> bool {
    let Ok(pid) = u32::try_from(leader) else {
        return false;
    };
    job_table().get(&pid).is_some_and(|job| {
        // SAFETY: a job handle the table owns, open while it is locked.
        unsafe { TerminateJobObject(job.as_raw_handle(), KILLED_EXIT_CODE as u32) != 0 }
    })
}

/// Interrupt a job — the Ctrl-C a user would type. Returns whether it was sent.
///
/// A PTY job: `\x03` typed at its console. ConPTY turns it into a CTRL_C_EVENT
/// for every process attached, as a tty's line discipline turns it into SIGINT.
///
/// A pipe job: CTRL_BREAK_EVENT to its process group (it leads one,
/// `CREATE_NEW_PROCESS_GROUP`). Windows cannot address a CTRL_C_EVENT to one
/// group — it reaches every process on the console or none — so the break is the
/// one interrupt a single job can be sent (python sees it as `SIGBREAK`). It
/// reaches the job only through a console the kernel shares with it.
pub(crate) fn interrupt(pid: u32, pty: Option<&PtyMaster>) -> bool {
    match pty {
        Some(m) => write_handle(&m.input, b"\x03").is_ok(),
        // SAFETY: plain values; a group that is gone fails the call.
        None => unsafe { GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT, pid) != 0 },
    }
}

/// python's `Process.returncode`: the process exit code (Windows has no signal
/// deaths to report as `-N`).
pub(crate) fn exit_code(s: ExitStatus) -> Option<i32> {
    s.code()
}

/// Whether a failed [`spawn`] failed on its working directory rather than its
/// program: `CreateProcessW` checks `lpCurrentDirectory` first and answers
/// `ERROR_DIRECTORY` for one that is missing (or not a directory).
pub(crate) fn cwd_failed(e: &io::Error, _cwd: &str) -> bool {
    e.raw_os_error() == Some(ERROR_DIRECTORY as i32)
}

// ── pipes ───────────────────────────────────────────────────────────

/// What a pipe-reading thread hands over per message (python's pipe granularity).
const PIPE_CHUNK: usize = 64 * 1024;
/// How many chunks may wait unread before the thread stops reading the pipe.
const PIPE_CHUNKS: usize = 4;

/// One anonymous pipe, `(read end, write end)`, neither inheritable.
fn pipe() -> io::Result<(OwnedHandle, OwnedHandle)> {
    let (mut r, mut w): (HANDLE, HANDLE) = (null_mut(), null_mut());
    // SAFETY: valid out-pointers; null attributes = not inheritable.
    if unsafe { CreatePipe(&mut r, &mut w, null(), 0) } == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: CreatePipe handed us two fresh handles we now own.
    Ok(unsafe {
        (
            OwnedHandle::from_raw_handle(r),
            OwnedHandle::from_raw_handle(w),
        )
    })
}

/// The read end of a pipe — a pipe job's stdout or stderr, a terminal's output —
/// as an async reader.
///
/// A dedicated thread does the blocking `ReadFile`s and hands each chunk over a
/// BOUNDED channel. Bounded is the point: when nobody reads (a paused terminal, a
/// pump that is behind), the thread blocks on the channel, stops reading the
/// pipe, the pipe fills, and the program blocks on its next write — what an unread
/// pipe does on unix. A plain thread, for the reason [`watch_exit`] gives.
pub struct PipeReader {
    rx: mpsc::Receiver<io::Result<Vec<u8>>>,
    chunk: Vec<u8>,
    at: usize,
}

impl PipeReader {
    fn spawn(read_end: OwnedHandle) -> io::Result<Self> {
        let (tx, rx) = mpsc::channel(PIPE_CHUNKS);
        std::thread::Builder::new()
            .name("exec_bridge pipe".to_string())
            .spawn(move || drain(&read_end, &tx))?;
        Ok(Self {
            rx,
            chunk: Vec::new(),
            at: 0,
        })
    }
}

/// The pipe-reading thread's body: chunks until EOF (every writer closed), an
/// error (handed on, then the end), or nobody left to read them.
fn drain(read_end: &OwnedHandle, tx: &mpsc::Sender<io::Result<Vec<u8>>>) {
    let mut buf = vec![0u8; PIPE_CHUNK];
    loop {
        let mut n = 0u32;
        // SAFETY: a buffer of its own length on a handle this thread owns; a
        // synchronous read (no OVERLAPPED).
        let ok = unsafe {
            ReadFile(
                read_end.as_raw_handle(),
                buf.as_mut_ptr(),
                buf.len() as u32,
                &mut n,
                null_mut(),
            )
        } != 0;
        let item = if ok {
            if n == 0 {
                continue; // a zero-length write, not the end
            }
            Ok(buf[..n as usize].to_vec())
        } else {
            let e = io::Error::last_os_error();
            if e.raw_os_error() == Some(ERROR_BROKEN_PIPE as i32) {
                return; // every writer closed: EOF, not an error
            }
            Err(e)
        };
        let failed = item.is_err();
        if tx.blocking_send(item).is_err() || failed {
            return;
        }
    }
}

impl AsyncRead for PipeReader {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let me = self.get_mut();
        if me.at == me.chunk.len() {
            match std::task::ready!(me.rx.poll_recv(cx)) {
                Some(Ok(chunk)) => {
                    me.chunk = chunk;
                    me.at = 0;
                }
                Some(Err(e)) => return Poll::Ready(Err(e)),
                None => return Poll::Ready(Ok(())), // EOF: nothing read
            }
        }
        let n = buf.remaining().min(me.chunk.len() - me.at);
        buf.put_slice(&me.chunk[me.at..me.at + n]);
        me.at += n;
        Poll::Ready(Ok(()))
    }
}

/// Write ALL of `data` to a synchronous handle.
fn write_handle(h: &OwnedHandle, data: &[u8]) -> io::Result<usize> {
    let mut written = 0usize;
    while written < data.len() {
        let rest = &data[written..];
        let mut n = 0u32;
        // SAFETY: a buffer of at least `len` bytes on a handle we own; synchronous.
        let ok = unsafe {
            WriteFile(
                h.as_raw_handle(),
                rest.as_ptr(),
                u32::try_from(rest.len()).unwrap_or(u32::MAX),
                &mut n,
                null_mut(),
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        written += n as usize;
    }
    Ok(written)
}

// ── terminals (ConPTY) ──────────────────────────────────────────────

/// A ConPTY pseudoconsole, shared by the terminal's two ends and the job's exit
/// watcher, and closed ONCE: by the watcher when the job's process exits, or —
/// no job ever ran on it — when the last holder drops it.
struct PseudoConsole(Mutex<Option<HPCON>>);

impl PseudoConsole {
    fn lock(&self) -> MutexGuard<'_, Option<HPCON>> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn handle(&self) -> io::Result<HPCON> {
        self.lock().ok_or_else(console_closed)
    }

    fn resize(&self, size: COORD) -> io::Result<()> {
        let hpc = self.handle()?;
        // SAFETY: a live pseudoconsole (closing it takes it out under the lock
        // first, and `hpc` is a copy of a value that stays valid until then).
        hresult(unsafe { ResizePseudoConsole(hpc, size) })
    }

    /// Close it. This can block until its last output is read (ConPTY flushes a
    /// final frame), so it runs off the async runtime: on the exit watcher.
    fn close(&self) {
        let hpc = self.lock().take();
        if let Some(hpc) = hpc {
            // SAFETY: taken out of the lock, so closed exactly once.
            unsafe { ClosePseudoConsole(hpc) };
        }
    }
}

impl Drop for PseudoConsole {
    fn drop(&mut self) {
        // Still open: no job ran on it (its spawn failed). Closed on a thread of
        // its own, for the reason `close` gives.
        let hpc = self
            .0
            .get_mut()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        if let Some(hpc) = hpc {
            let _ = std::thread::Builder::new()
                .name("exec_bridge pty close".to_string())
                // SAFETY: taken out, so closed exactly once.
                .spawn(move || unsafe { ClosePseudoConsole(hpc) });
        }
    }
}

fn console_closed() -> io::Error {
    io::Error::new(
        io::ErrorKind::BrokenPipe,
        "exec_bridge: the pseudoconsole is closed",
    )
}

/// An HRESULT as an `io::Error` (std prints and maps the HRESULT itself).
fn hresult(hr: i32) -> io::Result<()> {
    if hr < 0 {
        return Err(io::Error::from_raw_os_error(hr));
    }
    Ok(())
}

/// A console size: ConPTY counts cells in an `i16`.
fn coord(cols: u16, rows: u16) -> io::Result<COORD> {
    let too_big = |_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "a console is at most 32767 cells on a side",
        )
    };
    Ok(COORD {
        X: i16::try_from(cols).map_err(too_big)?,
        Y: i16::try_from(rows).map_err(too_big)?,
    })
}

/// The kernel's end of a job's terminal: the pseudoconsole's output (read by a
/// [`PipeReader`]) and input (written synchronously) pipes.
pub(crate) struct PtyMaster {
    output: tokio::sync::Mutex<PipeReader>,
    input: Arc<OwnedHandle>,
    console: Arc<PseudoConsole>,
}

/// The job's end of a terminal: the pseudoconsole itself, which [`spawn`]
/// attaches the job to.
pub(crate) struct PtySlave(Arc<PseudoConsole>);

/// A fresh pseudoconsole sized `cols`×`rows`.
pub(crate) fn open_pty(cols: u16, rows: u16) -> io::Result<(PtyMaster, PtySlave)> {
    let size = coord(cols, rows)?;
    let (in_r, in_w) = pipe()?;
    let (out_r, out_w) = pipe()?;
    let mut hpc: HPCON = 0;
    // SAFETY: two open pipe ends and a valid out-pointer.
    hresult(unsafe {
        CreatePseudoConsole(
            size,
            in_r.as_raw_handle(),
            out_w.as_raw_handle(),
            0,
            &mut hpc,
        )
    })?;
    let console = Arc::new(PseudoConsole(Mutex::new(Some(hpc))));
    // The console holds its own copies of its two ends now. Ours must close, or
    // the output pipe keeps a writer after the console is gone and never ends.
    drop((in_r, out_w));
    let output = PipeReader::spawn(out_r)?;
    Ok((
        PtyMaster {
            output: tokio::sync::Mutex::new(output),
            input: Arc::new(in_w),
            console: Arc::clone(&console),
        },
        PtySlave(console),
    ))
}

impl PtyMaster {
    /// One read of what the console shows. `Ok(0)` is the end of the terminal —
    /// the console closed with its job.
    pub(crate) async fn read(&self, buf: &mut [u8]) -> io::Result<usize> {
        self.output.lock().await.read(buf).await
    }

    /// Write `data` as typed input, all of it. The input pipe is synchronous, so
    /// the write runs on the blocking pool (the console reads its input
    /// continuously, so it does not stay there).
    pub(crate) async fn write(&self, data: &[u8]) -> io::Result<usize> {
        let input = Arc::clone(&self.input);
        let data = data.to_vec();
        tokio::task::spawn_blocking(move || write_handle(&input, &data))
            .await
            .map_err(io::Error::other)?
    }

    /// Set the console's size (the program sees a buffer-size event).
    pub(crate) fn resize(&self, cols: u16, rows: u16) -> io::Result<()> {
        self.console.resize(coord(cols, rows)?)
    }
}

// ── the pasted-image dir ────────────────────────────────────────────

/// python `tempfile.TMP_MAX`: how many names `mkdtemp` tries.
const TMP_MAX: usize = 10_000;

/// python `tempfile.mkdtemp(prefix=…)` — a PRIVATE dir (the `0700` contract: owned
/// by this user, a protected DACL granting only this user) in the system temp dir,
/// named `prefix` + 8 characters of python's `_RandomNameSequence` alphabet.
pub(crate) fn mkdtemp(prefix: &str) -> io::Result<PathBuf> {
    let sd = OwnerOnly::new()?;
    let sa = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: sd.0,
        bInheritHandle: 0,
    };
    let dir = std::env::temp_dir();
    let mut last = io::Error::from_raw_os_error(ERROR_ALREADY_EXISTS as i32);
    for _ in 0..TMP_MAX {
        let path = dir.join(format!("{prefix}{}", random_name()));
        let w = wide(path.as_os_str())?;
        // SAFETY: a NUL-terminated path; `sa` and its descriptor outlive the call.
        if unsafe { CreateDirectoryW(w.as_ptr(), &sa) } != 0 {
            return Ok(path);
        }
        let e = io::Error::last_os_error();
        if e.raw_os_error() != Some(ERROR_ALREADY_EXISTS as i32) {
            return Err(e);
        }
        last = e;
    }
    Err(last)
}

/// 8 characters of python's `tempfile._RandomNameSequence` alphabet.
fn random_name() -> String {
    const CHARS: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789_";
    let mut buf = [0u8; 8];
    fantastic_kernel::lifecycle::fill_random(&mut buf);
    buf.iter()
        .map(|b| char::from(CHARS[usize::from(*b) % CHARS.len()]))
        .collect()
}

/// A security descriptor that makes what it creates PRIVATE: owned by this
/// process's user, with a PROTECTED DACL (nothing inherited from the parent dir)
/// granting that user — and no one else — full access, inherited by what is
/// created inside. Freed on drop.
struct OwnerOnly(PSECURITY_DESCRIPTOR);

impl OwnerOnly {
    fn new() -> io::Result<Self> {
        let sid = user_sid()?;
        let sddl = wide(OsStr::new(&format!("O:{sid}D:P(A;OICI;FA;;;{sid})")))?;
        let mut sd: PSECURITY_DESCRIPTOR = null_mut();
        // SAFETY: a NUL-terminated SDDL string and a valid out-pointer.
        let ok = unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                sddl.as_ptr(),
                SDDL_REVISION_1,
                &mut sd,
                null_mut(),
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self(sd))
    }
}

impl Drop for OwnerOnly {
    fn drop(&mut self) {
        // SAFETY: allocated by ConvertStringSecurityDescriptorToSecurityDescriptorW.
        unsafe { LocalFree(self.0) };
    }
}

/// This process's user, as an SDDL SID string (`S-1-5-21-…`).
fn user_sid() -> io::Result<String> {
    let mut token: HANDLE = null_mut();
    // SAFETY: this process's pseudo-handle and a valid out-pointer.
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: a fresh handle we now own.
    let token = unsafe { OwnedHandle::from_raw_handle(token) };
    let mut len = 0u32;
    // SAFETY: a null buffer asks for the size (failing, by design).
    unsafe { GetTokenInformation(token.as_raw_handle(), TokenUser, null_mut(), 0, &mut len) };
    let mut buf = vec![0u64; (len as usize).div_ceil(8)];
    // SAFETY: an 8-aligned buffer of at least `len` bytes.
    let ok = unsafe {
        GetTokenInformation(
            token.as_raw_handle(),
            TokenUser,
            buf.as_mut_ptr().cast(),
            len,
            &mut len,
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the call filled the buffer with a TOKEN_USER.
    let user = unsafe { &*buf.as_ptr().cast::<TOKEN_USER>() };
    let mut text: PWSTR = null_mut();
    // SAFETY: the token's SID, valid while `buf` lives; a valid out-pointer.
    if unsafe { ConvertSidToStringSidW(user.User.Sid, &mut text) } == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: a NUL-terminated string the call allocated, freed right after.
    let sid = unsafe {
        let len = (0..).take_while(|i| *text.add(*i) != 0).count();
        let s = String::from_utf16_lossy(std::slice::from_raw_parts(text, len));
        LocalFree(text.cast());
        s
    };
    Ok(sid)
}

// ── test fixtures (the unix tests' shell scripts, in cmd) ───────────

/// TEST ONLY: the members of `leader`'s job other than the leader — its Job
/// Object's processes.
#[cfg(test)]
pub(crate) fn job_members(leader: u32) -> Vec<u32> {
    use windows_sys::Win32::System::JobObjects::{
        JobObjectBasicProcessIdList, QueryInformationJobObject, JOBOBJECT_BASIC_PROCESS_ID_LIST,
    };
    const MAX: usize = 1024;
    let jobs = job_table();
    let Some(job) = jobs.get(&leader) else {
        return Vec::new();
    };
    let bytes = std::mem::size_of::<JOBOBJECT_BASIC_PROCESS_ID_LIST>()
        + (MAX - 1) * std::mem::size_of::<usize>();
    let mut buf = vec![0usize; bytes.div_ceil(std::mem::size_of::<usize>())];
    // SAFETY: a pointer-aligned buffer of `bytes` bytes on a job we own.
    let ok = unsafe {
        QueryInformationJobObject(
            job.as_raw_handle(),
            JobObjectBasicProcessIdList,
            buf.as_mut_ptr().cast(),
            bytes as u32,
            null_mut(),
        )
    };
    assert!(ok != 0, "{}", io::Error::last_os_error());
    let list = buf.as_ptr().cast::<JOBOBJECT_BASIC_PROCESS_ID_LIST>();
    // SAFETY: the call filled the header and that many ids after it, in `buf`.
    let ids = unsafe {
        let n = (*list).NumberOfProcessIdsInList as usize;
        std::slice::from_raw_parts(std::ptr::addr_of!((*list).ProcessIdList).cast::<usize>(), n)
    };
    ids.iter()
        .map(|p| *p as u32)
        .filter(|p| *p != leader)
        .collect()
}

/// TEST ONLY: a script `dir/<stem>.cmd` that prints `text` and exits 0 (a bare
/// `<stem>` finds it through `PATHEXT`).
#[cfg(test)]
pub(crate) fn echo_script(dir: &Path, stem: &str, text: &str) -> PathBuf {
    script(dir, stem, &format!("@echo {text}\r\n"))
}

/// TEST ONLY: a stand-in for a login shell, `dir/<stem>.cmd`: it prints
/// `SHELL_UP <its args>`, then reads one typed line and prints `GOT <line>`.
#[cfg(test)]
pub(crate) fn fake_shell(dir: &Path, stem: &str) -> PathBuf {
    script(
        dir,
        stem,
        "@echo off\r\necho SHELL_UP %*\r\nset /p line=\r\necho GOT %line%\r\n",
    )
}

/// `dir/<stem>.cmd` holding `body`.
#[cfg(test)]
fn script(dir: &Path, stem: &str, body: &str) -> PathBuf {
    let path = dir.join(format!("{stem}.cmd"));
    std::fs::write(&path, body).expect("write the script");
    path
}

/// `%SystemRoot%`, and the env a cmd fixture runs in: `ping` needs Winsock, and
/// Winsock needs `SYSTEMROOT`.
#[cfg(test)]
fn system_env() -> (String, serde_json::Value) {
    let root = std::env::var("SYSTEMROOT").expect("SYSTEMROOT");
    let env = serde_json::json!({"PATH": format!("{root}\\System32"), "SYSTEMROOT": root});
    (format!("{root}\\System32\\cmd.exe"), env)
}

/// TEST ONLY: a PIPE job that launches two background processes of its own and
/// then waits — `(command, args, env)`.
#[cfg(test)]
pub(crate) fn launcher_job() -> (String, Vec<String>, serde_json::Value) {
    let (cmd, env) = system_env();
    let line = "start /b ping -n 303 127.0.0.1 >nul & start /b ping -n 304 127.0.0.1 >nul \
                & ping -n 305 127.0.0.1 >nul";
    (
        cmd,
        vec!["/d".to_string(), "/c".to_string(), line.to_string()],
        env,
    )
}

/// TEST ONLY: an interactive shell for a PTY job, and what to type into it to
/// launch two background processes and then wait — `(command, args, env, typed)`.
#[cfg(test)]
pub(crate) fn interactive_shell() -> (String, Vec<String>, serde_json::Value, String) {
    let (cmd, env) = system_env();
    let typed = "start /b ping -n 300 127.0.0.1 >nul\rstart /b ping -n 301 127.0.0.1 >nul\r\
                 ping -n 302 127.0.0.1 >nul\r";
    (cmd, vec!["/d".to_string()], env, typed.to_string())
}

/// TEST ONLY: what an interactive program takes as Enter — a console line ends
/// at a carriage return.
#[cfg(test)]
pub(crate) const ENTER: &str = "\r";

/// TEST ONLY: a pipe-job `(command, args)` that prints its environment's
/// `FT_SECRET_RS` and `FT_OPEN_RS` (`none` when unset) as `secret/open`.
#[cfg(test)]
pub(crate) fn print_env_job() -> (String, Vec<String>) {
    let py = which::which("python").expect("python on PATH");
    (
        py.to_string_lossy().to_string(),
        vec![
            "-c".to_string(),
            "import os; g = os.environ.get; print(g('FT_SECRET_RS', 'none') + '/' + \
             g('FT_OPEN_RS', 'none'))"
                .to_string(),
        ],
    )
}

/// TEST ONLY: the errno a spawn into a missing `cwd` fails with —
/// `ERROR_DIRECTORY`, python's `ENOTDIR`.
#[cfg(test)]
pub(crate) const MISSING_CWD_ERRNO: i32 = 20;

#[cfg(test)]
mod tests {
    use super::*;

    /// Twins of python's `subprocess.list2cmdline` outputs.
    #[test]
    fn list2cmdline_quotes_like_python() {
        let cases: [(&[&str], &str); 6] = [
            (&["a b c", "d", "e"], r#""a b c" d e"#),
            (&[r#"ab"c"#, "\\", "d"], r#"ab\"c \ d"#),
            (&[r#"ab"c"#, " \\", "d"], r#"ab\"c " \\" d"#),
            (&[r"a\\\b", "de fg", "h"], r#"a\\\b "de fg" h"#),
            (&[r#"a\"b"#, "c", "d"], r#"a\\\"b c d"#),
            (&["", "x"], r#""" x"#),
        ];
        for (argv, want) in cases {
            assert_eq!(list2cmdline(argv), want, "{argv:?}");
        }
    }

    #[test]
    fn a_later_env_pair_wins_whatever_its_case() {
        let env = [
            ("Path".to_string(), "a".to_string()),
            ("PATH".to_string(), "b".to_string()),
        ];
        let block = String::from_utf16(&env_block(&env).unwrap()).unwrap();
        assert_eq!(block, "PATH=b\0\0");
    }
}
