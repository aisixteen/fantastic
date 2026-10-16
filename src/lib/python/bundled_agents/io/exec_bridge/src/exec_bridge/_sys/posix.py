"""POSIX (macOS, Linux): sessions, signals, `socketpair`, a pty read from the
event loop."""

from __future__ import annotations

import asyncio
import ctypes
import fcntl
import os
import signal
import socket
import struct
import subprocess
import sys
import termios

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

#: What a child needs from the OS environment just to start: nothing here.
OS_ENV: tuple[str, ...] = ()

#: The child env var that names its shell: the login shell the account runs.
SHELL_VAR = "SHELL"
#: What makes that shell a LOGIN shell — the tail `shell: "login"` permits.
LOGIN_ARGS: tuple[str, ...] = ("-l",)


# ─── the control channel ────────────────────────────────────────


def ctrl_pair():
    """`socketpair()`: `(parent, child)`, connected, private to this process."""
    return socket.socketpair()


# ─── the PATH grammar ───────────────────────────────────────────


def names_a_file(command: str) -> bool:
    """A command with a `/` names a file — exec'd as written, never searched."""
    return "/" in command


def child_path(env: dict) -> str | None:
    """The child's `PATH`, or None."""
    return env.get("PATH")


def exe_names(command: str) -> list[str]:
    """The names a bare command is found under in a `PATH` dir: itself."""
    return [command]


def is_executable(full: str) -> bool:
    """A regular file this user may execute."""
    return os.path.isfile(full) and os.access(full, os.X_OK)


# ─── jobs ───────────────────────────────────────────────────────


def _all_pids() -> list[int]:
    """Every pid the OS lists right now — `proc_listallpids` on macOS, `/proc` on
    Linux. Picked by platform, statically; there is no third source."""
    if sys.platform == "darwin":
        libc = ctypes.CDLL(None)
        n = libc.proc_listallpids(None, 0)
        buf = (ctypes.c_int * (n + 64))()
        got = libc.proc_listallpids(buf, ctypes.sizeof(buf))
        return [buf[i] for i in range(max(got, 0)) if buf[i] > 0]
    return [int(d) for d in os.listdir("/proc") if d.isdigit()]


def session_members(leader: int) -> list[int]:
    """Every process of the session `leader` leads, but the leader — found by
    session id, so a member that left the leader's process group (a shell's own
    job, a `nohup`) is found too."""
    out = []
    for pid in _all_pids():
        if pid == leader:
            continue
        try:
            if os.getsid(pid) == leader:
                out.append(pid)
        except (ProcessLookupError, PermissionError):
            continue  # gone meanwhile, or not ours to see
    return out


def kill_session(leader: int) -> None:
    """SIGKILL a job's WHOLE session: every member first, then the leader.

    Every job leads its own session (`start_new_session`, pipe and PTY alike).
    What it launched stays in that session — `server &` from a script, a `nohup`,
    and each job an interactive shell puts in its OWN process group, which
    neither a kill of the leader nor a tty hangup (foreground group only)
    reaches. Once the leader is gone they reparent to init, so the members are
    found by session id BEFORE the leader dies. A process that `setsid()`s
    itself out of the session leaves it by design: containment beyond that is
    the OS's (devops), not this bridge's."""
    for pid in session_members(leader):
        try:
            os.kill(pid, signal.SIGKILL)
        except (ProcessLookupError, PermissionError):
            continue  # gone meanwhile, or not ours to kill
    try:
        os.kill(leader, signal.SIGKILL)
    except ProcessLookupError:
        pass


def interrupt(process, term) -> None:
    """SIGINT to the job's process — a PTY job's included (its leader)."""
    process.send_signal(signal.SIGINT)


def pid_alive(pid: int) -> bool:
    """`kill(pid, 0)`: True while the process exists, ours or not."""
    try:
        os.kill(pid, 0)
    except ProcessLookupError:
        return False
    except PermissionError:
        return True
    return True


def popen_inheriting(argv, sock, **kwargs) -> subprocess.Popen:
    """`subprocess.Popen(argv, **kwargs)` whose child inherits `sock` at its own
    number (`pass_fds`) — the fixture a test spawns a wire-speaking program with,
    no job around it."""
    return subprocess.Popen(argv, pass_fds=[sock.fileno()], **kwargs)


async def spawn_argv(argv, executable: str, cwd: str, env: dict, pass_fd: int):
    """`executable` (argv[0] as given) started piped, in its own session, with
    `pass_fd` inherited and nothing else of the kernel's."""
    return await asyncio.create_subprocess_exec(
        *argv,
        executable=executable,
        cwd=cwd,
        stdout=asyncio.subprocess.PIPE,
        stderr=asyncio.subprocess.PIPE,
        env=env,
        pass_fds=(pass_fd,),
        start_new_session=True,
        # A line longer than asyncio's 64 KiB default would end the whole pipe with
        # `[pump error]`; a program's line is not bounded.
        limit=16 * 1024 * 1024,
    )


def _controlling_tty() -> None:
    """In the child, after `setsid`: make the PTY slave (fd 0) the controlling
    terminal, so job control, Ctrl-C and SIGWINCH reach the program."""
    fcntl.ioctl(0, termios.TIOCSCTTY, 0)


async def spawn_pty(argv, executable: str, cwd: str, env: dict, pass_fd: int, term):
    """`executable` started on `term`'s slave (its stdio), leading a new session
    with that tty as its controlling terminal."""
    try:
        process = await asyncio.create_subprocess_exec(
            *argv,
            executable=executable,
            cwd=cwd,
            stdin=term.slave,
            stdout=term.slave,
            stderr=term.slave,
            env=env,
            pass_fds=(pass_fd,),
            start_new_session=True,
            preexec_fn=_controlling_tty,
        )
        os.set_blocking(term.master, False)
    finally:
        # The master sees EOF only once every slave copy is closed — the kernel's
        # goes now, spawned or not.
        term.release_slave()
    return process


# ─── the terminal ───────────────────────────────────────────────


def _set_winsize(fd: int, cols: int, rows: int) -> None:
    fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", rows, cols, 0, 0))


class Pty:
    """A pseudo-terminal: the job's SLAVE end (its stdio and controlling tty, until
    `spawn_pty` hands it over) and the kernel's MASTER end, read from the event
    loop (`add_reader`) and written without blocking it."""

    def __init__(self, cols: int, rows: int) -> None:
        self.master, self.slave = os.openpty()
        try:
            _set_winsize(self.master, cols, rows)
        except BaseException:
            self.close()
            raise

    def release_slave(self) -> None:
        """Close the kernel's copy of the slave end (the job has its own)."""
        if self.slave is not None:
            os.close(self.slave)
            self.slave = None

    def start_reading(self, on_data) -> None:
        """From now on `on_data(chunk)` runs on the loop, in the context of THIS
        call, as output arrives — `b""` once, at EOF (or EIO: the slave closed with
        the child). Idempotent."""
        asyncio.get_running_loop().add_reader(self.master, self._readable, on_data)

    def _readable(self, on_data) -> None:
        try:
            data = os.read(self.master, 4096)
        except BlockingIOError:
            return
        except OSError:
            data = b""
        on_data(data)

    def stop_reading(self) -> None:
        """No more `on_data` until the next `start_reading` — the tty fills and the
        program throttles."""
        asyncio.get_running_loop().remove_reader(self.master)

    async def write(self, buf: bytes) -> int:
        """Write the FULL buffer to the non-blocking master fd. `os.write`
        short-writes and raises `BlockingIOError` when the tty buffer is full; one
        unchecked write drops the tail — a bracketed paste then loses its end
        marker and the shell sticks in paste mode. Loop until every byte lands."""
        loop = asyncio.get_running_loop()
        written = 0
        while written < len(buf):
            try:
                written += os.write(self.master, buf[written:])
            except BlockingIOError:
                ev = asyncio.Event()
                loop.add_writer(self.master, ev.set)
                try:
                    await ev.wait()
                finally:
                    loop.remove_writer(self.master)
        return written

    def resize(self, cols: int, rows: int) -> None:
        """Set the window size; the tty sends the job SIGWINCH."""
        _set_winsize(self.master, cols, rows)

    def close(self) -> None:
        """Close the kernel's ends (the master — the job's tty hangs up)."""
        self.release_slave()
        os.close(self.master)


def open_pty(cols: int, rows: int) -> Pty:
    return Pty(cols, rows)
