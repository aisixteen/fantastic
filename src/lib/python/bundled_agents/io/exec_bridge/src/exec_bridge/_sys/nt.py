"""Windows: kernel32 through `ctypes` (rust `fantastic_exec_bridge::sys`'s Windows
half — the same choices, the same values).

- A job is spawned SUSPENDED, put into its OWN Job Object, then resumed — so
  everything it launches is born inside that job, and "kill the session" is
  `TerminateJobObject`: the Job Object is Windows' session. A killed job's exit
  code is 1 (`TerminateProcess(h, 1)`, python's `Popen.kill`); Windows has no
  signal numbers to negate.
- What a job inherits is named exactly: `PROC_THREAD_ATTRIBUTE_HANDLE_LIST` lists
  the control socket and the job's stdio, and no other handle of this process
  crosses. The control socket is a loopback TCP pair (there is no `socketpair`);
  the connector's `socket.socket(fileno=…)` takes the inherited SOCKET, whose
  number is the same in the child.
- A pipe job leads its own process group (`CREATE_NEW_PROCESS_GROUP`): its
  `interrupt` is a CTRL_BREAK to that group, and a Ctrl-C at the kernel's console
  does not reach it.
- A PTY job runs on a ConPTY pseudoconsole; the kernel's end is two anonymous
  pipes — synchronous, nothing the event loop can poll — so the output is drained
  by a thread that hands each chunk to the loop. Its `interrupt` is `\\x03` typed
  into the console.
- A spawn's failure reads as POSIX python prints it, naming the program — or the
  `cwd`, when that is what failed (`ERROR_DIRECTORY`: `[Errno 20] Not a
  directory`).
"""

from __future__ import annotations

import asyncio
import contextlib
import contextvars
import ctypes
import os
import signal
import socket
import subprocess
import threading
import weakref
from ctypes import wintypes

from kernel._sys import posix_error
from kernel._sys.nt import kernel32, last_error

__all__ = [
    "OS_ENV",
    "LOGIN_ARGS",
    "Pty",
    "SHELL_VAR",
    "child_path",
    "ctrl_pair",
    "exe_names",
    "interrupt",
    "is_executable",
    "kill_session",
    "names_a_file",
    "open_pty",
    "pid_alive",
    "popen_inheriting",
    "session_members",
    "spawn_argv",
    "spawn_pty",
]

#: What a child needs from the OS environment just to start: `SYSTEMROOT` —
#: without it Winsock cannot start in the child (WSAStartup fails, WinError 10106),
#: so its connector has no socket. Every job gets it (`tools._child_env`).
OS_ENV: tuple[str, ...] = ("SYSTEMROOT",)

#: The child env var that names its shell: `COMSPEC`, the command interpreter
#: every Windows environment names (`SHELL` is a unix convention, unset here).
SHELL_VAR = "COMSPEC"
#: What makes that shell a LOGIN shell: nothing — `cmd.exe` has no login mode (a
#: console session is already the user's), so `shell: "login"` permits the same
#: line as `shell: true`.
LOGIN_ARGS: tuple[str, ...] = ()

_HANDLE = wintypes.HANDLE
_DWORD = wintypes.DWORD
_BOOL = wintypes.BOOL


def _declare(fn, argtypes, restype):
    fn.argtypes = argtypes
    fn.restype = restype


class _COORD(ctypes.Structure):
    _fields_ = [("X", ctypes.c_short), ("Y", ctypes.c_short)]


class _STARTUPINFOW(ctypes.Structure):
    _fields_ = [
        ("cb", _DWORD),
        ("lpReserved", wintypes.LPWSTR),
        ("lpDesktop", wintypes.LPWSTR),
        ("lpTitle", wintypes.LPWSTR),
        ("dwX", _DWORD),
        ("dwY", _DWORD),
        ("dwXSize", _DWORD),
        ("dwYSize", _DWORD),
        ("dwXCountChars", _DWORD),
        ("dwYCountChars", _DWORD),
        ("dwFillAttribute", _DWORD),
        ("dwFlags", _DWORD),
        ("wShowWindow", wintypes.WORD),
        ("cbReserved2", wintypes.WORD),
        ("lpReserved2", ctypes.c_void_p),
        ("hStdInput", _HANDLE),
        ("hStdOutput", _HANDLE),
        ("hStdError", _HANDLE),
    ]


class _STARTUPINFOEXW(ctypes.Structure):
    _fields_ = [("StartupInfo", _STARTUPINFOW), ("lpAttributeList", ctypes.c_void_p)]


class _PROCESS_INFORMATION(ctypes.Structure):
    _fields_ = [
        ("hProcess", _HANDLE),
        ("hThread", _HANDLE),
        ("dwProcessId", _DWORD),
        ("dwThreadId", _DWORD),
    ]


_declare(
    kernel32.CreatePipe,
    [ctypes.POINTER(_HANDLE)] * 2 + [ctypes.c_void_p, _DWORD],
    _BOOL,
)
_declare(
    kernel32.ReadFile,
    [_HANDLE, ctypes.c_void_p, _DWORD, ctypes.POINTER(_DWORD), ctypes.c_void_p],
    _BOOL,
)
_declare(
    kernel32.WriteFile,
    [_HANDLE, ctypes.c_void_p, _DWORD, ctypes.POINTER(_DWORD), ctypes.c_void_p],
    _BOOL,
)
_declare(
    kernel32.CreatePseudoConsole,
    [_COORD, _HANDLE, _HANDLE, _DWORD, ctypes.POINTER(ctypes.c_void_p)],
    ctypes.c_long,
)
_declare(kernel32.ResizePseudoConsole, [ctypes.c_void_p, _COORD], ctypes.c_long)
_declare(kernel32.ClosePseudoConsole, [ctypes.c_void_p], None)
_declare(
    kernel32.InitializeProcThreadAttributeList,
    [ctypes.c_void_p, _DWORD, _DWORD, ctypes.POINTER(ctypes.c_size_t)],
    _BOOL,
)
_declare(
    kernel32.UpdateProcThreadAttribute,
    [
        ctypes.c_void_p,
        _DWORD,
        ctypes.c_size_t,
        ctypes.c_void_p,
        ctypes.c_size_t,
        ctypes.c_void_p,
        ctypes.c_void_p,
    ],
    _BOOL,
)
_declare(kernel32.DeleteProcThreadAttributeList, [ctypes.c_void_p], None)
_declare(
    kernel32.CreateProcessW,
    [
        wintypes.LPCWSTR,
        ctypes.c_void_p,
        ctypes.c_void_p,
        ctypes.c_void_p,
        _BOOL,
        _DWORD,
        ctypes.c_void_p,
        wintypes.LPCWSTR,
        ctypes.c_void_p,
        ctypes.c_void_p,
    ],
    _BOOL,
)
_declare(kernel32.ResumeThread, [_HANDLE], _DWORD)
_declare(kernel32.WaitForSingleObject, [_HANDLE, _DWORD], _DWORD)
_declare(kernel32.GetExitCodeProcess, [_HANDLE, ctypes.POINTER(_DWORD)], _BOOL)
_declare(kernel32.OpenProcess, [_DWORD, _BOOL, _DWORD], _HANDLE)
_declare(kernel32.TerminateProcess, [_HANDLE, wintypes.UINT], _BOOL)
_declare(kernel32.CreateJobObjectW, [ctypes.c_void_p, wintypes.LPCWSTR], _HANDLE)
_declare(kernel32.AssignProcessToJobObject, [_HANDLE, _HANDLE], _BOOL)
_declare(kernel32.TerminateJobObject, [_HANDLE, wintypes.UINT], _BOOL)
_declare(
    kernel32.QueryInformationJobObject,
    [_HANDLE, ctypes.c_int, ctypes.c_void_p, _DWORD, ctypes.POINTER(_DWORD)],
    _BOOL,
)
_ntdll = ctypes.WinDLL("ntdll")
# Resumes every thread of a process — the one of a process created suspended, whose
# thread handle `subprocess` does not keep (psutil's `resume` is the same call).
_declare(_ntdll.NtResumeProcess, [_HANDLE], ctypes.c_long)
_declare(_ntdll.RtlNtStatusToDosError, [ctypes.c_long], wintypes.ULONG)

_CREATE_SUSPENDED = 0x00000004
_CREATE_NEW_PROCESS_GROUP = 0x00000200
_CREATE_UNICODE_ENVIRONMENT = 0x00000400
_EXTENDED_STARTUPINFO_PRESENT = 0x00080000
_STARTF_USESTDHANDLES = 0x00000100
_PROC_THREAD_ATTRIBUTE_HANDLE_LIST = 0x00020002
_PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE = 0x00020016
_PROCESS_TERMINATE = 0x0001
_PROCESS_SET_QUOTA = 0x0100
_PROCESS_SUSPEND_RESUME = 0x0800
_PROCESS_QUERY_LIMITED_INFORMATION = 0x1000
_STILL_ACTIVE = 259
_INFINITE = 0xFFFFFFFF
_WAIT_FAILED = 0xFFFFFFFF
_ERROR_ACCESS_DENIED = 5
_ERROR_DIRECTORY = 267
_JOB_OBJECT_BASIC_PROCESS_ID_LIST = 3
#: The exit code a killed job reports — python's `Popen.kill()`.
_KILLED_EXIT_CODE = 1
#: The largest console side ConPTY takes (a COORD is two shorts).
_MAX_CELLS = 32767


# ─── the control channel ────────────────────────────────────────


def ctrl_pair():
    """A CONNECTED loopback TCP pair `(parent, child)` — Windows has no
    `socketpair`. Listen on 127.0.0.1:0, connect, accept — and keep the accepted
    end only when its peer is exactly our connecting socket: any local process
    may connect to a listening loopback port, and one that raced in ahead of ours
    would otherwise be handed a job's control channel. Such a connection is
    dropped and the accept repeats (ours is already queued — `connect` returned).
    The listener closes the moment the pair exists. `TCP_NODELAY` on both ends:
    the wire is small frames, and Nagle plus a delayed ACK would hold a frame
    behind an unacknowledged one for up to 200 ms."""
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as listener:
        listener.bind(("127.0.0.1", 0))
        listener.listen()
        child = socket.create_connection(listener.getsockname())
        try:
            ours = child.getsockname()
            while True:
                parent, peer = listener.accept()
                if peer == ours:
                    break
                parent.close()
        except BaseException:
            child.close()
            raise
    for end in (parent, child):
        end.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
    return parent, child


# ─── the PATH grammar (python 3.11 `shutil.which` on Windows) ───

#: python `shutil._WIN_DEFAULT_PATHEXT`: what `which` tries when PATHEXT is unset.
_DEFAULT_PATHEXT = ".COM;.EXE;.BAT;.CMD;.VBS;.JS;.WS;.MSC"


def names_a_file(command: str) -> bool:
    """A command with a `/` or a `\\\\` names a file — run as written, never
    searched."""
    return "/" in command or "\\" in command


def child_path(env: dict) -> str | None:
    """The child's `PATH`, or None. Windows names are case-insensitive: `Path`
    is `PATH` (the last spelling given wins, as in the child's block)."""
    for k, v in reversed(env.items()):
        if k.upper() == "PATH":
            return v
    return None


def exe_names(command: str) -> list[str]:
    """The names a bare command may be found under in a `PATH` dir — `which`'s:
    the name as written when it already ends in a `PATHEXT` extension, else the
    name with each extension appended. `PATHEXT` is the KERNEL's (`which` reads
    `os.environ`): which extensions run is the OS's judgement, as `X_OK` is on
    POSIX — not the child's."""
    exts = [e for e in (os.environ.get("PATHEXT") or _DEFAULT_PATHEXT).split(";") if e]
    low = command.lower()
    if any(low.endswith(e.lower()) for e in exts):
        return [command]
    return [command + e for e in exts]


def is_executable(full: str) -> bool:
    """Any regular file — Windows has no execute bit."""
    return os.path.isfile(full)


def _env(env: dict) -> dict:
    """The child's block: Windows names are case-insensitive, so a later pair
    REPLACES an earlier one whatever its case, and the block is sorted by name
    (as `CreateProcessW` documents)."""
    named = {}
    for k, v in env.items():
        named[k.upper()] = (k, v)
    return dict(named[u] for u in sorted(named))


# ─── jobs ───────────────────────────────────────────────────────

# Every live job's Job Object handle, by the job's pid — what `kill_session`
# terminates. An entry lives as long as the job's process object: that object
# holds the process handle that keeps Windows from reusing the pid meanwhile.
_JOBS: dict[int, int] = {}


def _forget_job(pid: int, job: int) -> None:
    if _JOBS.get(pid) == job:
        del _JOBS[pid]
    kernel32.CloseHandle(job)


def _new_job() -> int:
    """A fresh Job Object, unnamed, no limits (closing it kills nothing: a job's
    processes outlive the kernel as a POSIX session does)."""
    job = kernel32.CreateJobObjectW(None, None)
    if not job:
        raise last_error()
    return job


def _contain(pid: int, job: int) -> None:
    """Put the SUSPENDED process `pid` into `job`, then let it run — everything it
    launches is born inside. (The caller holds the process's own handle, so the
    pid names it.)"""
    h = kernel32.OpenProcess(
        _PROCESS_SET_QUOTA | _PROCESS_TERMINATE | _PROCESS_SUSPEND_RESUME, False, pid
    )
    if not h:
        raise last_error()
    try:
        if not kernel32.AssignProcessToJobObject(job, h):
            raise last_error()
        status = _ntdll.NtResumeProcess(h)
        if status < 0:
            raise last_error(code=_ntdll.RtlNtStatusToDosError(status))
    finally:
        kernel32.CloseHandle(h)


def _file(e: OSError, executable: str, cwd: str) -> OSError:
    """The spawn failure as POSIX python prints it, naming what failed: the `cwd`
    (`ERROR_DIRECTORY`), else the program."""
    culprit = cwd if e.winerror == _ERROR_DIRECTORY else executable
    return posix_error(OSError(None, e.strerror, culprit, e.winerror))


async def spawn_argv(argv, executable: str, cwd: str, env: dict, pass_fd: int):
    """`executable` (argv[0] as given) started piped in its own process group and
    its own Job Object, inheriting `pass_fd` (a SOCKET) and its stdio — nothing
    else."""
    job = _new_job()
    try:
        si = subprocess.STARTUPINFO()
        si.lpAttributeList = {"handle_list": [pass_fd]}
        # inheritable for this one call only — the handle list keeps it from any
        # other process this one starts meanwhile
        os.set_handle_inheritable(pass_fd, True)
        try:
            process = await asyncio.create_subprocess_exec(
                *argv,
                executable=executable,
                cwd=cwd,
                stdout=asyncio.subprocess.PIPE,
                stderr=asyncio.subprocess.PIPE,
                env=_env(env),
                startupinfo=si,
                creationflags=_CREATE_SUSPENDED | _CREATE_NEW_PROCESS_GROUP,
                limit=16 * 1024 * 1024,
            )
        except OSError as e:
            if e.winerror is None:
                raise
            raise _file(e, executable, cwd) from None
        finally:
            os.set_handle_inheritable(pass_fd, False)
        try:
            _contain(process.pid, job)
        except BaseException:
            # it never ran: gone before anything is told it started
            with contextlib.suppress(ProcessLookupError, OSError):
                process.kill()
            await process.wait()
            raise
    except BaseException:
        kernel32.CloseHandle(job)
        raise
    _JOBS[process.pid] = job
    weakref.finalize(process, _forget_job, process.pid, job)
    return process


def popen_inheriting(argv, sock, **kwargs) -> subprocess.Popen:
    """`subprocess.Popen(argv, **kwargs)` whose child inherits `sock` (its SOCKET
    handle, at the same number) and its stdio, nothing else — the fixture a test
    spawns a wire-speaking program with, no job around it."""
    si = subprocess.STARTUPINFO()
    si.lpAttributeList = {"handle_list": [sock.fileno()]}
    os.set_handle_inheritable(sock.fileno(), True)
    try:
        return subprocess.Popen(argv, startupinfo=si, **kwargs)
    finally:
        os.set_handle_inheritable(sock.fileno(), False)


def kill_session(leader: int) -> None:
    """`TerminateJobObject` on the job's own Job Object: the job and everything
    it launched, exit code 1."""
    job = _JOBS.get(leader)
    if job is not None:
        kernel32.TerminateJobObject(job, _KILLED_EXIT_CODE)


def interrupt(process, term) -> None:
    """A PTY job: `\\x03` typed into its console (the console turns it into the
    Ctrl-C of whatever reads it). A pipe job: CTRL_BREAK to its process group."""
    if term is not None:
        term.interrupt()
    else:
        process.send_signal(signal.CTRL_BREAK_EVENT)


def pid_alive(pid: int) -> bool:
    """True while the process exists, ours or not: it opens and has not exited,
    or it exists and refuses us the handle. (Never `os.kill(pid, 0)` — on Windows
    that TERMINATES the process.)"""
    h = kernel32.OpenProcess(_PROCESS_QUERY_LIMITED_INFORMATION, False, pid)
    if not h:
        return ctypes.get_last_error() == _ERROR_ACCESS_DENIED
    try:
        code = _DWORD()
        return bool(
            kernel32.GetExitCodeProcess(h, ctypes.byref(code))
            and code.value == _STILL_ACTIVE
        )
    finally:
        kernel32.CloseHandle(h)


def session_members(leader: int) -> list[int]:
    """Every process in the Job Object of the job `leader` started, but the
    leader."""
    job = _JOBS.get(leader)
    if job is None:
        return []

    class _IdList(ctypes.Structure):
        # JOBOBJECT_BASIC_PROCESS_ID_LIST, room for 1024 ids
        _fields_ = [
            ("NumberOfAssignedProcesses", _DWORD),
            ("NumberOfProcessIdsInList", _DWORD),
            ("ProcessIdList", ctypes.c_size_t * 1024),
        ]

    ids = _IdList()
    if not kernel32.QueryInformationJobObject(
        job,
        _JOB_OBJECT_BASIC_PROCESS_ID_LIST,
        ctypes.byref(ids),
        ctypes.sizeof(ids),
        None,
    ):
        raise last_error()
    return [
        int(p)
        for p in ids.ProcessIdList[: ids.NumberOfProcessIdsInList]
        if int(p) != leader
    ]


# ─── PTY jobs: ConPTY ───────────────────────────────────────────


def _pipe() -> tuple[int, int]:
    """An anonymous pipe `(read, write)`, neither end inheritable."""
    r, w = _HANDLE(), _HANDLE()
    if not kernel32.CreatePipe(ctypes.byref(r), ctypes.byref(w), None, 0):
        raise last_error()
    return r.value, w.value


def _coord(cols: int, rows: int) -> _COORD:
    if not (0 < cols <= _MAX_CELLS and 0 < rows <= _MAX_CELLS):
        raise ValueError("a console is at most 32767 cells on a side")
    return _COORD(cols, rows)


def _hresult(hr: int) -> None:
    if hr < 0:
        # a FACILITY_WIN32 HRESULT carries its Win32 code in the low word
        code = hr & 0xFFFF if (hr & 0xFFFF0000) == 0x80070000 else hr
        raise last_error(code=code)


class Pty:
    """A ConPTY pseudoconsole and the kernel's two pipe ends: `_in` (what is
    typed) and `_out` (what the console renders), the latter drained by a thread
    that hands each chunk to the event loop — in the context `start_reading` was
    called in, as `add_reader` would."""

    def __init__(self, cols: int, rows: int) -> None:
        size = _coord(cols, rows)
        in_r, in_w = _pipe()
        out_r, out_w = _pipe()
        hpc = ctypes.c_void_p()
        hr = kernel32.CreatePseudoConsole(size, in_r, out_w, 0, ctypes.byref(hpc))
        # the console keeps its own copies of its ends
        kernel32.CloseHandle(in_r)
        kernel32.CloseHandle(out_w)
        if hr < 0:
            kernel32.CloseHandle(in_w)
            kernel32.CloseHandle(out_r)
            _hresult(hr)
        self._hpc = hpc.value
        self._in = in_w
        self._out = out_r
        # `_hpc` and `_in` close once, whoever gets there first (the exit
        # watcher, or `close`); a write holds `_io` so `_in` never closes under it.
        self._lock = threading.Lock()
        self._io = threading.Lock()
        self._go = threading.Event()  # set: the reader may read
        self._on_data = None
        self._context = None
        self._loop = None
        self._reader = None

    # the kernel's side

    def start_reading(self, on_data) -> None:
        """From now on `on_data(chunk)` runs on the loop, in the context of THIS
        call, as output arrives — `b""` once, at EOF. Idempotent."""
        self._on_data = on_data
        self._context = contextvars.copy_context()
        if self._reader is None:
            self._loop = asyncio.get_running_loop()
            self._reader = threading.Thread(
                target=self._drain, name="exec_bridge pty", daemon=True
            )
            self._reader.start()
        self._go.set()

    def stop_reading(self) -> None:
        """No more reads until the next `start_reading` — the pipe fills and the
        program throttles."""
        self._go.clear()

    def _drain(self) -> None:
        """The reader thread: one chunk at a time, until the console is gone (its
        end of the pipe closed — `ERROR_BROKEN_PIPE`, or any other failure: EOF).
        Each chunk is HANDLED on the loop before the next read starts, as one
        `add_reader` callback is: a `stop_reading` it makes holds from the very
        next read on, never one chunk late."""
        buf = ctypes.create_string_buffer(4096)
        n = _DWORD()
        handled = threading.Event()
        while True:
            self._go.wait()
            ok = kernel32.ReadFile(self._out, buf, len(buf), ctypes.byref(n), None)
            if ok and not n.value:
                continue  # a zero-length write, not the end
            data = buf.raw[: n.value] if ok else b""
            handled.clear()
            try:
                self._loop.call_soon_threadsafe(
                    self._deliver, data, handled, context=self._context
                )
            except RuntimeError:
                break  # the loop is closed: nobody left to tell
            while not handled.wait(0.5):
                if self._loop.is_closed():
                    break
            if not data:
                break
        kernel32.CloseHandle(self._out)

    def _deliver(self, data: bytes, handled: threading.Event) -> None:
        try:
            if self._on_data is not None:
                self._on_data(data)
        finally:
            handled.set()

    async def write(self, buf: bytes) -> int:
        """Write the FULL buffer to the console's input — on a worker thread, the
        pipe is synchronous."""
        return await asyncio.get_running_loop().run_in_executor(
            None, self._write_all, bytes(buf)
        )

    def _write_all(self, data: bytes) -> int:
        with self._io:
            if self._in is None:
                raise posix_error(BrokenPipeError(32, "Broken pipe"))
            written = 0
            n = _DWORD()
            while written < len(data):
                chunk = data[written:]
                if not kernel32.WriteFile(
                    self._in, chunk, len(chunk), ctypes.byref(n), None
                ):
                    raise last_error()
                written += n.value
            return written

    def interrupt(self) -> None:
        """`\\x03` typed into the console — its Ctrl-C."""
        self._write_all(b"\x03")

    def resize(self, cols: int, rows: int) -> None:
        """Set the console's size; the program sees a window-size event."""
        size = _coord(cols, rows)
        with self._lock:
            if self._hpc is None:
                raise BrokenPipeError(32, "Broken pipe")
            _hresult(kernel32.ResizePseudoConsole(self._hpc, size))

    def close(self) -> None:
        """Release the kernel's side: nothing more is delivered, the reader drains
        what is left to EOF, and the console closes (which also ends whatever is
        still attached to it — the tty hangup of POSIX). `ClosePseudoConsole` can
        wait for its output to be read, so it runs on its own thread."""
        self._on_data = None
        self._go.set()
        if self._reader is None:
            kernel32.CloseHandle(self._out)
            self._reader = False  # closed: no reader will ever start
        threading.Thread(
            target=self.close_console, name="exec_bridge pty close", daemon=True
        ).start()

    # the job's side

    def console(self):
        """The pseudoconsole a job is spawned on (None once closed)."""
        return self._hpc

    def close_console(self) -> None:
        """Close the pseudoconsole, then the input pipe — once. ConPTY keeps its
        output pipe open for as long as the console lives, so this is what lets
        the reader reach EOF once the job is gone."""
        with self._lock:
            hpc, self._hpc = self._hpc, None
        if hpc is None:
            return
        kernel32.ClosePseudoConsole(hpc)
        with self._io:
            kernel32.CloseHandle(self._in)
            self._in = None


def open_pty(cols: int, rows: int) -> Pty:
    return Pty(cols, rows)


class _ConsoleProcess:
    """A job started on a pseudoconsole — the part of asyncio's `Process` this
    bundle uses (`pid`, `returncode`, `wait`), over the process handle. A thread
    waits on the handle; then the console is closed (as the slave's last close is
    on POSIX), so the terminal reaches EOF."""

    def __init__(self, pid: int, handle: int, term: Pty) -> None:
        self.pid = pid
        self.returncode = None
        loop = asyncio.get_running_loop()
        self._exited = loop.create_future()
        weakref.finalize(self, kernel32.CloseHandle, handle)
        threading.Thread(
            target=self._watch,
            args=(loop, handle, term),
            name="exec_bridge job",
            daemon=True,
        ).start()

    def _watch(self, loop, handle: int, term: Pty) -> None:
        code = _DWORD()
        if kernel32.WaitForSingleObject(handle, _INFINITE) == _WAIT_FAILED or not (
            kernel32.GetExitCodeProcess(handle, ctypes.byref(code))
        ):
            code.value = _KILLED_EXIT_CODE  # a wait that failed reads as killed
        try:
            loop.call_soon_threadsafe(self._exit, code.value)
        except RuntimeError:
            pass  # the loop is closed: nobody left to tell
        term.close_console()

    def _exit(self, code: int) -> None:
        self.returncode = code
        if not self._exited.done():
            self._exited.set_result(code)

    async def wait(self) -> int:
        return await asyncio.shield(self._exited)


class _AttrList:
    """A `PROC_THREAD_ATTRIBUTE_LIST` for `count` attributes. The values it
    points at must outlive the `CreateProcessW` that reads them: kept here."""

    def __init__(self, count: int) -> None:
        size = ctypes.c_size_t()
        kernel32.InitializeProcThreadAttributeList(None, count, 0, ctypes.byref(size))
        self.buf = ctypes.create_string_buffer(size.value)
        if not kernel32.InitializeProcThreadAttributeList(
            self.buf, count, 0, ctypes.byref(size)
        ):
            raise last_error()
        self._keep = []

    def set(self, attribute: int, value, size: int) -> None:
        self._keep.append(value)
        if not kernel32.UpdateProcThreadAttribute(
            self.buf, 0, attribute, value, size, None, None
        ):
            raise last_error()

    def delete(self) -> None:
        kernel32.DeleteProcThreadAttributeList(self.buf)


def _env_block(env: dict):
    """`NAME=value\\0…\\0`, UTF-16 — the block `CREATE_UNICODE_ENVIRONMENT` takes."""
    pairs = [f"{k}={v}" for k, v in _env(env).items()]
    if any("\0" in p for p in pairs):
        raise ValueError("embedded null character")
    return ctypes.create_unicode_buffer("".join(p + "\0" for p in pairs) + "\0")


async def spawn_pty(argv, executable: str, cwd: str, env: dict, pass_fd: int, term):
    """`executable` started on `term`'s pseudoconsole, in its own Job Object,
    inheriting `pass_fd` (a SOCKET) — nothing else."""
    hpc = term.console()
    if hpc is None:
        raise BrokenPipeError(32, "Broken pipe")
    job = _new_job()
    try:
        si = _STARTUPINFOEXW()
        si.StartupInfo.cb = ctypes.sizeof(si)
        # The std handles are ALWAYS named here: unset, the job would pick up the
        # kernel's own (a redirected kernel's log file) instead of its console;
        # left null, a console program attaching to its pseudoconsole gets the
        # console's.
        si.StartupInfo.dwFlags = _STARTF_USESTDHANDLES
        attrs = _AttrList(2)
        handles = (_HANDLE * 1)(pass_fd)
        attrs.set(_PROC_THREAD_ATTRIBUTE_HANDLE_LIST, handles, ctypes.sizeof(handles))
        # the attribute's value IS the HPCON, not a pointer to one
        attrs.set(
            _PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE,
            ctypes.c_void_p(hpc),
            ctypes.sizeof(ctypes.c_void_p),
        )
        si.lpAttributeList = ctypes.addressof(attrs.buf)
        cmdline = ctypes.create_unicode_buffer(subprocess.list2cmdline(argv))
        block = _env_block(env)
        pi = _PROCESS_INFORMATION()
        os.set_handle_inheritable(pass_fd, True)
        try:
            created = kernel32.CreateProcessW(
                executable,
                cmdline,
                None,
                None,
                True,
                _CREATE_SUSPENDED
                | _CREATE_UNICODE_ENVIRONMENT
                | _EXTENDED_STARTUPINFO_PRESENT,
                block,
                cwd,
                ctypes.byref(si),
                ctypes.byref(pi),
            )
            code = ctypes.get_last_error()
        finally:
            os.set_handle_inheritable(pass_fd, False)
            attrs.delete()
        if not created:
            e = OSError(None, ctypes.FormatError(code), None, code)
            raise _file(e, executable, cwd)
        try:
            if not kernel32.AssignProcessToJobObject(job, pi.hProcess):
                e = last_error()
                kernel32.TerminateProcess(pi.hProcess, _KILLED_EXIT_CODE)
                raise e
            if kernel32.ResumeThread(pi.hThread) == _WAIT_FAILED:
                e = last_error()
                kernel32.TerminateJobObject(job, _KILLED_EXIT_CODE)
                raise e
        except BaseException:
            kernel32.CloseHandle(pi.hProcess)
            raise
        finally:
            kernel32.CloseHandle(pi.hThread)
    except BaseException:
        kernel32.CloseHandle(job)
        raise
    process = _ConsoleProcess(pi.dwProcessId, pi.hProcess, term)
    _JOBS[process.pid] = job
    weakref.finalize(process, _forget_job, process.pid, job)
    return process
