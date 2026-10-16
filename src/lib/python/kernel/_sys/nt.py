"""Windows: kernel32 through `ctypes`, on the C runtime's own descriptors.

The workdir lock is a `LockFileEx` range lock on ONE byte at 2^62 — rust
`fantastic_kernel::sys` locks the same byte the same way, so a python and a rust
kernel exclude each other. Windows range locks are MANDATORY (a locked byte
cannot be read by anyone else), so the lock sits far past the end of the file and
the pid the file holds stays readable to whoever is refused. Like `flock`, the
lock belongs to the open handle.

The lock protocol renames and unlinks `lock.yaml` while descriptors on it are
open (python `_lock._publish`), which POSIX always allows. Windows allows it only
when every open handle shares DELETE — python's own `os.open` never does — and
replaces a name that is open only under POSIX rename semantics: hence
`open_lock` and `replace` below.

An `OSError` CPython raises here carries a Win32 code and prints
`[WinError N] <system text>`; the wire reads `[Errno N] <C runtime text>` on every
OS (`posix_error`, rust `fantastic_bundle::errno_of` / `strerror`).
"""

from __future__ import annotations

import ctypes
import errno
import functools
import msvcrt
import os
import re
import signal
import sys
from ctypes import wintypes

__all__ = [
    "STOP_SIGNALS",
    "install_stop_signals",
    "lock",
    "open_lock",
    "posix_error",
    "posix_errors",
    "remove_stop_signals",
    "replace",
    "stdin_is_terminal",
    "try_lock",
    "unlock_close",
    "utf8_stdio",
]

# The names below without a leading underscore and outside `__all__` are shared
# with the other packages' Windows halves (`file_bridge._sys.nt`,
# `exec_bridge._sys.nt`) — one declaration of each Win32 call.
kernel32 = ctypes.WinDLL("kernel32", use_last_error=True)


def stdin_is_terminal() -> bool:
    """Is stdin a CONSOLE — a human (or a ConPTY) to run the REPL for? Not
    `isatty()`: on Windows that is true of any character device, `NUL` included,
    so a daemon spawned with stdin=DEVNULL would start a REPL on it. A console
    handle is the one whose mode can be read."""
    mode = wintypes.DWORD()
    handle = msvcrt.get_osfhandle(sys.stdin.fileno()) if sys.stdin else -1
    return handle != -1 and bool(kernel32.GetConsoleMode(handle, ctypes.byref(mode)))


kernel32.CreateFileW.argtypes = [
    wintypes.LPCWSTR,
    wintypes.DWORD,
    wintypes.DWORD,
    wintypes.LPVOID,
    wintypes.DWORD,
    wintypes.DWORD,
    wintypes.HANDLE,
]
kernel32.CreateFileW.restype = wintypes.HANDLE
kernel32.CloseHandle.argtypes = [wintypes.HANDLE]
kernel32.CloseHandle.restype = wintypes.BOOL
kernel32.LockFileEx.argtypes = [
    wintypes.HANDLE,
    wintypes.DWORD,
    wintypes.DWORD,
    wintypes.DWORD,
    wintypes.DWORD,
    wintypes.LPVOID,
]
kernel32.LockFileEx.restype = wintypes.BOOL
kernel32.UnlockFileEx.argtypes = [
    wintypes.HANDLE,
    wintypes.DWORD,
    wintypes.DWORD,
    wintypes.DWORD,
    wintypes.LPVOID,
]
kernel32.UnlockFileEx.restype = wintypes.BOOL
kernel32.SetFileInformationByHandle.argtypes = [
    wintypes.HANDLE,
    ctypes.c_int,
    wintypes.LPVOID,
    wintypes.DWORD,
]
kernel32.SetFileInformationByHandle.restype = wintypes.BOOL

GENERIC_READ = 0x80000000
GENERIC_WRITE = 0x40000000
DELETE = 0x00010000
READ_CONTROL = 0x00020000
WRITE_DAC = 0x00040000
FILE_READ_ATTRIBUTES = 0x80
SHARE_ALL = 0x1 | 0x2 | 0x4  # FILE_SHARE_READ | WRITE | DELETE
CREATE_NEW = 1
OPEN_EXISTING = 3
OPEN_ALWAYS = 4
FILE_ATTRIBUTE_NORMAL = 0x80
FILE_FLAG_BACKUP_SEMANTICS = 0x02000000
FILE_FLAG_OPEN_REPARSE_POINT = 0x00200000
INVALID_HANDLE = ctypes.c_void_p(-1).value
_LOCKFILE_FAIL_IMMEDIATELY = 0x1
_LOCKFILE_EXCLUSIVE_LOCK = 0x2
_ERROR_LOCK_VIOLATION = 33
_FILE_RENAME_INFO_EX = 22  # FILE_INFO_BY_HANDLE_CLASS FileRenameInfoEx
_FILE_RENAME_FLAG_REPLACE_IF_EXISTS = 0x1
_FILE_RENAME_FLAG_POSIX_SEMANTICS = 0x2

#: The one byte the workdir lock covers: 2^62, past any file this lock writes.
_LOCK_OFFSET = 1 << 62


class _OVERLAPPED(ctypes.Structure):
    _fields_ = [
        ("Internal", ctypes.c_size_t),
        ("InternalHigh", ctypes.c_size_t),
        ("Offset", wintypes.DWORD),
        ("OffsetHigh", wintypes.DWORD),
        ("hEvent", wintypes.HANDLE),
    ]


# ─── OS errors on the wire ──────────────────────────────────────


def _strerror(n: int) -> str:
    """The C runtime's text for errno `n`. A Winsock errno (python's
    `errno.ECONNRESET` & co. are the Winsock codes on Windows) has none, so it
    reads as CPython prints the system's message — trailing dots and line ends
    cut (rust `fantastic_bundle::strerror`)."""
    if n >= 10000:
        return re.sub(r"[\x00- .]+$", "", ctypes.FormatError(n))
    return os.strerror(n)


def posix_error(e: BaseException) -> BaseException:
    """`e` as POSIX python prints it: an `OSError` CPython built from a Win32 code
    becomes the same class with `[Errno N] <C runtime text>` and the same file
    names; anything else is `e` itself. The errno is the one CPython already
    mapped the code to (`PC/errmap.h` `winerror_to_errno`), so the class — chosen
    by errno — does not change."""
    if not isinstance(e, OSError) or e.winerror is None:
        return e
    # Built from (errno, text, filename) ONLY: any 4th argument — even None —
    # fills the `winerror` slot, and `str()` then prints `[WinError None]`.
    shaped = OSError(e.errno, _strerror(e.errno), e.filename)
    if e.filename2 is not None:
        shaped.filename2 = e.filename2
    return shaped


def posix_errors(fn):
    """Every `OSError` `fn` raises reads as POSIX python prints it (`posix_error`);
    the rest of what it raises, untouched."""

    @functools.wraps(fn)
    def wrapper(*args, **kwargs):
        try:
            return fn(*args, **kwargs)
        except OSError as e:
            shaped = posix_error(e)
            if shaped is e:
                raise
            raise shaped from None

    return wrapper


def last_error(filename=None, filename2=None, code: int | None = None) -> OSError:
    """The `OSError` for this thread's last Win32 error (or `code`), POSIX-shaped."""
    code = ctypes.get_last_error() if code is None else code
    return posix_error(
        OSError(None, ctypes.FormatError(code), filename, code, filename2)
    )


def create_file(path, access: int, disposition: int, flags: int = 0) -> int:
    """`CreateFileW` sharing read, write AND delete (as rust's `OpenOptions` does),
    so the handle never stands in the way of a rename or unlink of its file. A
    failure raises the POSIX-shaped `OSError` naming `path`."""
    name = os.fspath(path)
    h = kernel32.CreateFileW(
        name, access, SHARE_ALL, None, disposition, flags or FILE_ATTRIBUTE_NORMAL, None
    )
    if h is None or h == INVALID_HANDLE:
        raise last_error(name)
    return h


# ─── the process's own text ─────────────────────────────────────


def utf8_stdio() -> None:
    """stdout/stderr write UTF-8 lines ending in `\\n`, as the rust kernel's do. A
    redirected stream (the e2e harness' log, a pipe) would otherwise be the ANSI
    code page with `\\r\\n` line ends: the stop line's `—` written as cp1252, a
    `→` in a refusal raising on stdout."""
    for stream in (sys.stdout, sys.stderr):
        stream.reconfigure(encoding="utf-8", newline="\n")


# ─── the workdir lock ───────────────────────────────────────────


def open_lock(path, flags: int, mode: int = 0o777) -> int:
    """`os.open` for a lock file, sharing DELETE: its descriptor never stands in
    the way of renaming or unlinking the file (the POSIX rule the lock protocol
    leans on). `mode` sets nothing — a Windows file has no permission bits, and
    every mode the lock passes is writable (python's own `os.open` would only set
    the read-only flag)."""
    write = flags & (os.O_WRONLY | os.O_RDWR)
    access = GENERIC_READ | (GENERIC_WRITE if write else 0)
    if flags & os.O_CREAT:
        disposition = CREATE_NEW if flags & os.O_EXCL else OPEN_ALWAYS
    else:
        disposition = OPEN_EXISTING
    h = create_file(path, access, disposition)
    try:
        # binary: no `O_TEXT`, so `os.read`/`os.write` move the bytes as they are
        return msvcrt.open_osfhandle(h, 0 if write else os.O_RDONLY)
    except BaseException:
        kernel32.CloseHandle(h)
        raise


def _range() -> _OVERLAPPED:
    return _OVERLAPPED(Offset=_LOCK_OFFSET & 0xFFFFFFFF, OffsetHigh=_LOCK_OFFSET >> 32)


def lock(fd: int) -> None:
    """Blocking exclusive `LockFileEx` on the lock byte of `fd`."""
    ov = _range()
    if not kernel32.LockFileEx(
        msvcrt.get_osfhandle(fd), _LOCKFILE_EXCLUSIVE_LOCK, 0, 1, 0, ctypes.byref(ov)
    ):
        raise last_error()


def try_lock(fd: int) -> None:
    """Non-blocking exclusive `LockFileEx`; held elsewhere (another process, or
    another handle of this one — `flock`'s per-open rule) raises
    `BlockingIOError`."""
    ov = _range()
    if kernel32.LockFileEx(
        msvcrt.get_osfhandle(fd),
        _LOCKFILE_EXCLUSIVE_LOCK | _LOCKFILE_FAIL_IMMEDIATELY,
        0,
        1,
        0,
        ctypes.byref(ov),
    ):
        return
    code = ctypes.get_last_error()
    if code == _ERROR_LOCK_VIOLATION:
        raise BlockingIOError(errno.EAGAIN, os.strerror(errno.EAGAIN))
    raise last_error(code=code)


def unlock_close(fd: int) -> None:
    """Unlock the lock byte, then close `fd`. A lock left to `CloseHandle` is
    released when the system gets to it (the `LockFileEx` docs), so a racer could
    still find it held; unlocking first frees it now. A descriptor that holds no
    lock has nothing to unlock — that call's failure is the answer, not an error."""
    ov = _range()
    kernel32.UnlockFileEx(msvcrt.get_osfhandle(fd), 0, 1, 0, ctypes.byref(ov))
    os.close(fd)


def replace(src, dst) -> None:
    """`src` takes `dst`'s name, atomically, whoever has `dst` open (with DELETE
    shared) — `rename(2)`. `os.replace` (`MoveFileExW`) refuses a target that is
    open at all; POSIX rename semantics (`FileRenameInfoEx`, as rust's
    `std::fs::rename`) do not. The link itself is renamed, never followed."""
    src_name = os.fspath(src)
    h = create_file(
        src_name,
        DELETE,
        OPEN_EXISTING,
        FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS,
    )
    try:
        target = os.path.abspath(os.fspath(dst))
        units = len(target.encode("utf-16-le")) // 2  # UTF-16 code units

        class _RenameInfo(ctypes.Structure):
            # FILE_RENAME_INFO: the `Flags` union, the root (none — `target` is
            # absolute), the name's length in bytes, the name itself.
            _fields_ = [
                ("Flags", wintypes.DWORD),
                ("RootDirectory", wintypes.HANDLE),
                ("FileNameLength", wintypes.DWORD),
                ("FileName", wintypes.WCHAR * (units + 1)),
            ]

        info = _RenameInfo(
            Flags=_FILE_RENAME_FLAG_REPLACE_IF_EXISTS
            | _FILE_RENAME_FLAG_POSIX_SEMANTICS,
            RootDirectory=None,
            FileNameLength=units * 2,
            FileName=target,
        )
        if not kernel32.SetFileInformationByHandle(
            h, _FILE_RENAME_INFO_EX, ctypes.byref(info), ctypes.sizeof(info)
        ):
            raise last_error(src_name, target)
    finally:
        kernel32.CloseHandle(h)


# ─── the daemon's stop signals ──────────────────────────────────

#: The signals that stop the daemon, by name — the name is what its
#: `[kernel] <name> — shutting down...` line prints. SIGBREAK is CTRL_BREAK, the
#: polite stop another process sends a process group on Windows (the e2e harness'
#: `stop_group`): it stops the kernel exactly as SIGTERM does on POSIX. No other
#: process can deliver SIGTERM here; it stays, so a `raise_signal` behaves.
STOP_SIGNALS = ("SIGINT", "SIGTERM", "SIGBREAK")


def _deliver(loop, on_stop, name, signum, frame) -> None:
    # A python-level handler runs on the main thread between bytecodes, after the
    # proactor loop's wakeup fd (`signal.set_wakeup_fd`, set by `run_forever`)
    # has already woken it — hand the stop to the loop, like `add_signal_handler`.
    if not loop.is_closed():
        loop.call_soon_threadsafe(on_stop, name)


def install_stop_signals(loop, on_stop) -> None:
    """From now on each of `STOP_SIGNALS` runs `on_stop(<its name>)` on `loop`.
    Windows event loops have no `add_signal_handler`; a `signal.signal` handler
    that schedules onto the loop is the same delivery."""
    for name in STOP_SIGNALS:
        signal.signal(
            getattr(signal, name), functools.partial(_deliver, loop, on_stop, name)
        )


def remove_stop_signals(loop) -> None:
    """Undo `install_stop_signals` (each signal back to its default action)."""
    for name in STOP_SIGNALS:
        signal.signal(getattr(signal, name), signal.SIG_DFL)
