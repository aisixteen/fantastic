"""POSIX (macOS, Linux): `fcntl.flock`, the event loop's own signal handlers, and
OSErrors exactly as the OS hands them over."""

from __future__ import annotations

import fcntl
import os
import sys
import signal

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

# ─── the terminal ───────────────────────────────────────────────


def stdin_is_terminal() -> bool:
    """Is stdin a terminal — a human (or a PTY) to run the REPL for?"""
    return sys.stdin.isatty()


# ─── the workdir lock ───────────────────────────────────────────


def open_lock(path, flags: int, mode: int = 0o777) -> int:
    """`os.open` for a lock file. A POSIX descriptor never stands in the way of
    renaming or unlinking its file, so this is `os.open` itself."""
    return os.open(path, flags, mode)


def lock(fd: int) -> None:
    """Blocking exclusive `flock(LOCK_EX)` on `fd`."""
    fcntl.flock(fd, fcntl.LOCK_EX)


def try_lock(fd: int) -> None:
    """Non-blocking exclusive `flock(LOCK_EX | LOCK_NB)`; held elsewhere raises
    `BlockingIOError`."""
    fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)


def unlock_close(fd: int) -> None:
    """Close `fd`; the flock it holds goes with it."""
    os.close(fd)


def replace(src, dst) -> None:
    """`rename(2)`: `src` takes `dst`'s name, atomically, whoever has `dst` open."""
    os.rename(src, dst)


# ─── the daemon's stop signals ──────────────────────────────────

#: The signals that stop the daemon, by name — the name is what its
#: `[kernel] <name> — shutting down...` line prints.
STOP_SIGNALS = ("SIGINT", "SIGTERM", "SIGHUP")


def install_stop_signals(loop, on_stop) -> None:
    """From now on each of `STOP_SIGNALS` runs `on_stop(<its name>)` on `loop`
    (`loop.add_signal_handler`: delivered through the loop's wakeup fd, never
    into the middle of a callback)."""
    for name in STOP_SIGNALS:
        loop.add_signal_handler(getattr(signal, name), on_stop, name)


def remove_stop_signals(loop) -> None:
    """Undo `install_stop_signals` (each signal back to its default action)."""
    for name in STOP_SIGNALS:
        loop.remove_signal_handler(getattr(signal, name))


# ─── the process's own text ─────────────────────────────────────


def utf8_stdio() -> None:
    """stdout/stderr write UTF-8 lines ending in `\\n`, as the rust kernel's do —
    already so here (a UTF-8 locale; PEP 538/540 coerce the C one)."""


# ─── OS errors on the wire ──────────────────────────────────────


def posix_error(e: BaseException) -> BaseException:
    """`e` as POSIX python prints it — on POSIX, `e` itself."""
    return e


def posix_errors(fn):
    """Every `OSError` `fn` raises reads as POSIX python prints it — on POSIX,
    `fn` itself."""
    return fn
