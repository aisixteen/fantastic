"""proc — THE single process-spawn surface for the whole kernel.

Every `create_subprocess_exec` for a command goes through here, exactly as every
`open()` goes through `file_bridge.fs`. That is what makes the ALLOWLIST unbypassable:
there is no second way to start a process, so there is no way around the check. `fs`
earns the running-dir law by being the only opener; `proc` earns the allowlist by being
the only spawner.

## The gate lives at the caller

The command gate is a REGEX allowlist over the full command line, applied by the caller
(`tools._command_permitted` against the record's `allowed`) BEFORE argv reaches here.
`spawn_argv` adds nothing to it: no per-call override, no `allow_any`, no "try the
allowlist, else fall back to whatever is on PATH". A per-call escape hatch is a
fallback, and a fallback that can grant arbitrary local code execution is the one this
kernel least wants. There is no default command either — the leg runs whatever the
operator allowlisted and nothing when the list is empty. "By default no defaults."

## What this does NOT claim

An allowlisted command is a *named* command, not a sandbox. A python it permits still
has `open()`, `socket` and `subprocess`. What the allowlist buys is that the operator
chose WHICH commands may run rather than the kernel guessing, and that a program cannot
pick a different one. Anything stronger needs an OS mechanism — `sandbox-exec`, a
separate uid, a container — none reachable in-process. Stated plainly because the
previous executor's security section was a documented hole, and replacing an honest hole
with a dishonest reassurance would be worse.
"""

from __future__ import annotations

import asyncio
import ctypes
import errno
import fcntl
import os
import signal
import sys
import termios


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
    for pid in _all_pids():
        if pid == leader:
            continue
        try:
            if os.getsid(pid) == leader:
                os.kill(pid, signal.SIGKILL)
        except (ProcessLookupError, PermissionError):
            continue  # gone meanwhile, or not ours to see
    try:
        os.kill(leader, signal.SIGKILL)
    except ProcessLookupError:
        pass


class NoPath(Exception):
    """A bare command, and the child's environment has no `PATH` to find it in."""


def resolve(command: str, cwd: str, env: dict) -> str:
    """argv[0] → the file to exec. A command with a `/` is exec'd as written
    (relative to `cwd`). A bare name is looked up in the CHILD's `PATH` only —
    never the kernel's, never a built-in default path; a child env without `PATH`
    raises `NoPath` (the operator names it in `inherit_env` or `env`)."""
    if "/" in command:
        return command
    path = env.get("PATH")
    if path is None:
        raise NoPath(command)
    for d in path.split(os.pathsep):
        # POSIX: an empty entry is the working directory — the CHILD's.
        full = os.path.abspath(os.path.join(cwd, d or ".", command))
        if os.path.isfile(full) and os.access(full, os.X_OK):
            return full
    raise FileNotFoundError(errno.ENOENT, os.strerror(errno.ENOENT), command)


async def spawn_argv(argv: list[str], cwd: str, env: dict, pass_fd: int):
    """Start ARGV as given, piped. The GATE is the caller's regex allowlist on the
    full command line (`tools._command_permitted`) — this function adds nothing to
    it. The child gets EXACTLY `env` (the caller composed it from the record: the
    inherited names, the operator's extras, the wire vars) — nothing leaks in from
    the kernel's own environment. `pass_fd` stays open across the exec so the child
    can speak the control wire (a command that does not know the wire simply never
    reads the fd). The child LEADS ITS OWN SESSION, so `kill_session` takes the
    command and everything it launched (`server &`, a `nohup`) — and a Ctrl-C at
    the kernel's terminal is the kernel's to handle, not a signal to every job."""
    return await asyncio.create_subprocess_exec(
        *argv,
        executable=resolve(argv[0], cwd, env),
        cwd=cwd,
        stdout=asyncio.subprocess.PIPE,
        stderr=asyncio.subprocess.PIPE,
        env=env,
        pass_fds=(pass_fd,),
        start_new_session=True,
        # A line longer than asyncio's 64 KiB default ended the whole pipe with
        # `[pump error]` (measured 2026-09-16); a program's line is not bounded.
        limit=16 * 1024 * 1024,
    )


def _controlling_tty() -> None:
    """In the child, after `setsid`: make the PTY slave (fd 0) the controlling
    terminal, so job control, Ctrl-C and SIGWINCH reach the program."""
    fcntl.ioctl(0, termios.TIOCSCTTY, 0)


async def spawn_pty(argv: list[str], cwd: str, env: dict, pass_fd: int, slave: int):
    """Start ARGV on a PTY: stdin/stdout/stderr are the SLAVE end, the child leads a
    new session with that tty as its controlling terminal. Same gate, same env rule,
    same control fd as `spawn_argv` — a terminal is a job, not a second spawner."""
    return await asyncio.create_subprocess_exec(
        *argv,
        executable=resolve(argv[0], cwd, env),
        cwd=cwd,
        stdin=slave,
        stdout=slave,
        stderr=slave,
        env=env,
        pass_fds=(pass_fd,),
        start_new_session=True,
        preexec_fn=_controlling_tty,
    )
