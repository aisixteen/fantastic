"""THE HARNESS'S OS BOUNDARY — the twin of the kernels' `sys` modules.

Every OS-specific call the launcher, the drivers, the runners and the probes
make is one small named function here. Callers never branch on the OS; only this
module does, once, at import.

The one idea worth stating: a spawned kernel (or a lane) is stopped WHOLE — it
and everything it started. On POSIX the child leads its own session and its
process group is signalled (`killpg`). On Windows the child leads its own
process group (`CREATE_NEW_PROCESS_GROUP`): a polite stop is a CTRL_BREAK to that
group, which every kernel treats as its SIGTERM, and a kill takes the whole
process tree (`taskkill /T /F`).
"""

from __future__ import annotations

import os
import signal
import subprocess
import sys
from contextlib import contextmanager
from pathlib import Path

WINDOWS = os.name == "nt"

if WINDOWS:
    import msvcrt
else:
    import fcntl


def exe(path: Path) -> Path:
    """A built binary's file: `fantastic` is `fantastic.exe` on Windows."""
    return path.with_name(path.name + ".exe") if WINDOWS else path


#: Python source a probe's PTY guest program starts with: its terminal (fd 0) in
#: RAW mode — every key and mouse report reaches `os.read(0, n)` as the bytes the
#: terminal sent, no line editing, no echo. POSIX: `tty.setraw`. Windows: the
#: console's VT input mode alone (ENABLE_VIRTUAL_TERMINAL_INPUT, every cooked
#: mode off), so ConPTY hands the terminal's VT bytes through.
RAW_TTY_PY = (
    "import ctypes, msvcrt\n"
    "ctypes.windll.kernel32.SetConsoleMode(msvcrt.get_osfhandle(0), 0x0200)\n"
    if WINDOWS
    else "import tty\ntty.setraw(0)\n"
)


def for_os(value):
    """A scenario value that differs by OS — a map `{posix: …, nt: …}` (a shell
    line: sh's `$((6*7))` and `export` are cmd.exe's `set /a` and `set`) — as
    this OS's; any other value as it is."""
    if isinstance(value, dict) and set(value) == {"posix", "nt"}:
        return value["nt" if WINDOWS else "posix"]
    return value


def venv_python(venv: Path) -> Path:
    """The interpreter of the virtualenv at `venv`."""
    return venv / "Scripts" / "python.exe" if WINDOWS else venv / "bin" / "python"


def group_kwargs() -> dict:
    """Spawn kwargs (`subprocess.Popen`, `asyncio.create_subprocess_exec`): the
    child leads its own process group, so it can be stopped whole."""
    if WINDOWS:
        return {"creationflags": subprocess.CREATE_NEW_PROCESS_GROUP}
    return {"start_new_session": True}


def stop_group(pid: int) -> None:
    """Ask the group `pid` leads to stop: SIGTERM, or CTRL_BREAK on Windows. A
    group already gone is not an error."""
    try:
        if WINDOWS:
            os.kill(pid, signal.CTRL_BREAK_EVENT)
        else:
            os.killpg(pid, signal.SIGTERM)
    except (ProcessLookupError, OSError):
        pass


def kill_group(pid: int) -> None:
    """Kill the group `pid` leads — and every process under it — now."""
    if WINDOWS:
        subprocess.run(
            ["taskkill", "/T", "/F", "/PID", str(pid)],
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )
        return
    try:
        os.killpg(pid, signal.SIGKILL)
    except ProcessLookupError:
        pass


def stop(proc: subprocess.Popen) -> None:
    """Ask ONE process spawned with [`group_kwargs`] to stop cleanly (its SIGTERM)."""
    if WINDOWS:
        stop_group(proc.pid)
    else:
        proc.send_signal(signal.SIGTERM)


def install_stop_handler(handler) -> None:
    """Run `handler` when this process is asked to stop (SIGTERM; on Windows also
    CTRL_BREAK, which is how [`stop_group`] asks), unless one is already set."""
    for sig in (signal.SIGTERM, signal.SIGBREAK) if WINDOWS else (signal.SIGTERM,):
        if signal.getsignal(sig) in (signal.SIG_DFL, None):
            signal.signal(sig, handler)


@contextmanager
def exclusive_lock(path: Path):
    """Hold an exclusive lock on the file at `path` for the `with` body —
    `flock` on POSIX, a byte-range lock on Windows (blocking either way)."""
    with open(path, "w") as f:
        if WINDOWS:
            while True:
                try:
                    msvcrt.locking(f.fileno(), msvcrt.LK_LOCK, 1)
                    break
                except OSError:
                    # LK_LOCK gives up after ten one-second tries; keep waiting.
                    continue
        else:
            fcntl.flock(f, fcntl.LOCK_EX)
        try:
            yield
        finally:
            if WINDOWS:
                f.seek(0)
                msvcrt.locking(f.fileno(), msvcrt.LK_UNLCK, 1)
            else:
                fcntl.flock(f, fcntl.LOCK_UN)


def make_private(path: Path) -> None:
    """Owner-only, as the kernels' `sys` modules judge it: `0700` / `0600` on POSIX;
    on Windows a protected DACL whose one grant is the current user (`icacls`:
    inheritance removed, the user's grant the only one)."""
    if not WINDOWS:
        os.chmod(path, 0o700 if path.is_dir() else 0o600)
        return
    sid = subprocess.run(
        ["whoami", "/user", "/fo", "csv", "/nh"], capture_output=True, text=True, check=True
    ).stdout.strip().split(",")[-1].strip('"')
    grant = f"*{sid}:(OI)(CI)F" if path.is_dir() else f"*{sid}:F"
    subprocess.run(
        ["icacls", str(path), "/inheritance:r", "/grant:r", grant],
        capture_output=True,
        check=True,
    )


def fake_command(directory: Path, name: str, mark: str) -> Path:
    """A stand-in program `name` in `directory` that prints `mark`, then echoes its
    input back line by line until EOF — found on PATH like the real one (a `sh`
    script on POSIX, a `.cmd` running this interpreter on Windows)."""
    if WINDOWS:
        echo = "import sys; [print(l, end='', flush=True) for l in sys.stdin]"
        fake = directory / f"{name}.cmd"
        fake.write_text(f'@echo off\r\necho {mark}\r\n"{sys.executable}" -u -c "{echo}"\r\n')
        return fake
    fake = directory / name
    fake.write_text(f"#!/bin/sh\necho {mark}\nexec cat\n")
    fake.chmod(0o755)
    return fake


def pid_alive(pid: int) -> bool:
    """Is process `pid` alive (ours or not)? POSIX `kill(pid, 0)`; on Windows the
    process opens and has not exited — `os.kill(pid, 0)` there TERMINATES it."""
    if not WINDOWS:
        try:
            os.kill(pid, 0)
        except ProcessLookupError:
            return False
        except PermissionError:
            return True
        return True
    import ctypes

    k32 = ctypes.WinDLL("kernel32", use_last_error=True)
    k32.OpenProcess.restype = ctypes.c_void_p
    h = k32.OpenProcess(0x1000, False, pid)  # PROCESS_QUERY_LIMITED_INFORMATION
    if not h:
        return ctypes.get_last_error() == 5  # ERROR_ACCESS_DENIED: exists, not ours
    code = ctypes.c_ulong()
    ok = k32.GetExitCodeProcess(ctypes.c_void_p(h), ctypes.byref(code))
    k32.CloseHandle(ctypes.c_void_p(h))
    return bool(ok) and code.value == 259  # STILL_ACTIVE


def child_env() -> dict:
    """The least environment a spawned interpreter needs: nothing on POSIX; on
    Windows `SYSTEMROOT` (without it the C runtime cannot start Winsock or find
    system DLLs)."""
    return {"SYSTEMROOT": os.environ["SYSTEMROOT"]} if WINDOWS else {}


def cpu_percent(*needles: str) -> float:
    """%CPU (one core = 100) summed over every process whose command line holds
    all of `needles` — `ps -A -o pcpu=,args=` on POSIX; on Windows the
    per-process performance counters, joined to command lines by pid."""
    if not WINDOWS:
        rows = subprocess.run(
            ["ps", "-A", "-o", "pcpu=,args="], capture_output=True, text=True
        ).stdout.splitlines()
        return sum(
            float(r.split(None, 1)[0])
            for r in rows
            if r.strip() and all(n in r for n in needles)
        )
    has = " -and ".join("$c.Contains('" + n.replace("'", "''") + "')" for n in needles)
    script = (
        "$ids = @(Get-CimInstance Win32_Process | Where-Object { $c = $_.CommandLine; "
        f"$c -and {has} }} | ForEach-Object {{ $_.ProcessId }}); "
        "if ($ids.Count -eq 0) { 0 } else { (Get-CimInstance "
        "Win32_PerfFormattedData_PerfProc_Process | Where-Object { $ids -contains "
        "$_.IDProcess } | Measure-Object PercentProcessorTime -Sum).Sum }"
    )
    out = subprocess.run(
        ["powershell", "-NoProfile", "-Command", script], capture_output=True, text=True
    ).stdout.strip()
    return float(out or 0)


def ensure_utf8() -> None:
    """Run the harness — and every process it starts — in python's UTF-8 mode
    where the locale's encoding is not UTF-8 (Windows: cp1252): every file the
    suite reads or writes is UTF-8. Runs this interpreter again, once, as a child
    (a Windows `execv` would end this process before the run does) and exits
    with its code. A UTF-8 locale (macOS, Linux) changes nothing."""
    import locale

    if sys.flags.utf8_mode or locale.getpreferredencoding(False).lower().replace("-", "") == "utf8":
        return
    os.environ["PYTHONUTF8"] = "1"
    raise SystemExit(subprocess.call([sys.executable, "-X", "utf8", *sys.argv]))


def lf_text_files() -> None:
    """Every text file this process writes through `Path.write_text` ends its
    lines with `\\n`, on every OS: fixtures are compared byte for byte with what a
    kernel reads back, and Windows' text mode would write `\\r\\n`. An explicit
    `newline=` still wins. POSIX writes `\\n` already: nothing changes there."""
    if not WINDOWS:
        return
    import pathlib

    orig = pathlib.Path.write_text
    if getattr(orig, "_lf", False):
        return

    def write_text(self, data, encoding=None, errors=None, newline="\n"):
        return orig(self, data, encoding, errors, newline)

    write_text._lf = True
    pathlib.Path.write_text = write_text


def program_pid(proc: subprocess.Popen, timeout: float = 10.0) -> int:
    """The pid of the PROGRAM `proc` runs. On POSIX `proc.pid`. On Windows a venv's
    `python.exe` is a launcher that starts the real interpreter as its one child:
    that child's pid (waited for, up to `timeout`)."""
    if not WINDOWS or Path(proc.args[0]).parent.name.lower() != "scripts":
        return proc.pid
    import ctypes
    import time
    from ctypes import wintypes

    class PROCESSENTRY32W(ctypes.Structure):
        _fields_ = [
            ("dwSize", wintypes.DWORD), ("cntUsage", wintypes.DWORD),
            ("th32ProcessID", wintypes.DWORD), ("th32DefaultHeapID", ctypes.c_size_t),
            ("th32ModuleID", wintypes.DWORD), ("cntThreads", wintypes.DWORD),
            ("th32ParentProcessID", wintypes.DWORD), ("pcPriClassBase", ctypes.c_long),
            ("dwFlags", wintypes.DWORD), ("szExeFile", ctypes.c_wchar * 260),
        ]

    k32 = ctypes.WinDLL("kernel32", use_last_error=True)
    k32.CreateToolhelp32Snapshot.restype = wintypes.HANDLE
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        snap = k32.CreateToolhelp32Snapshot(0x2, 0)  # TH32CS_SNAPPROCESS
        e = PROCESSENTRY32W()
        e.dwSize = ctypes.sizeof(e)
        child = None
        ok = k32.Process32FirstW(snap, ctypes.byref(e))
        while ok:
            if e.th32ParentProcessID == proc.pid:
                child = e.th32ProcessID
                break
            ok = k32.Process32NextW(snap, ctypes.byref(e))
        k32.CloseHandle(snap)
        if child is not None:
            return child
        time.sleep(0.05)
    return proc.pid
