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
