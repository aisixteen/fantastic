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

## What differs by OS

How a job is contained and killed, how it is interrupted, how its control channel and
its terminal are made, and what a bare command's PATH search tries live in `_sys` —
one module per OS family (POSIX: a session, `killpg`-style SIGKILL, `socketpair`, a
pty; Windows: a Job Object, CTRL_BREAK, a loopback TCP pair, ConPTY). Everything here
is OS-neutral; the rest of the bundle (and `py_agent`) sees only this module.
"""

from __future__ import annotations

import errno
import os

from exec_bridge import _sys

#: The terminal a PTY job runs on — `open_pty`'s, driven by `tools`.
Pty = _sys.Pty
#: The child env var naming the child's own shell (`SHELL`; Windows: `COMSPEC`).
SHELL_VAR = _sys.SHELL_VAR
#: The args that make that shell a login shell (`-l`; Windows: none).
LOGIN_ARGS = _sys.LOGIN_ARGS
#: What the OS needs in every child's environment (nothing on POSIX).
OS_ENV = _sys.OS_ENV


def ctrl_pair():
    """The control channel: a CONNECTED stream-socket pair `(parent, child)`. The
    relay serves `parent`; `child` is inherited by exactly one job, and its number
    (`child.fileno()`) is what `FANTASTIC_CTRL_FD` advertises — the connector's
    `socket.socket(fileno=…)` takes it on every OS."""
    return _sys.ctrl_pair()


def kill_session(leader: int) -> None:
    """SIGKILL a job's WHOLE session — the command and everything it launched — by
    the job's pid. POSIX: every member of the session the job leads, then the
    leader (`_sys`); Windows: the job's own Job Object."""
    _sys.kill_session(leader)


def interrupt(process, term: Pty | None) -> None:
    """The polite stop — a Ctrl-C — to one job: SIGINT on POSIX; on Windows a
    CTRL_BREAK to a pipe job's process group, `\x03` typed into a PTY job's
    console."""
    _sys.interrupt(process, term)


def open_pty(cols: int, rows: int) -> Pty:
    """A fresh terminal of `cols` x `rows`, for `spawn_pty`."""
    return _sys.open_pty(cols, rows)


class NoPath(Exception):
    """A bare command, and the child's environment has no `PATH` to find it in."""


def resolve(command: str, cwd: str, env: dict) -> str:
    """argv[0] → the file to exec. A command naming a file (a `/`; on Windows a `\\`
    too) is exec'd as written (relative to `cwd`). A bare name is looked up in the
    CHILD's `PATH` only — never the kernel's, never a built-in default path; a child
    env without `PATH` raises `NoPath` (the operator names it in `inherit_env` or
    `env`). Which names a bare command may be found under is the OS's (`_sys`: the
    name itself; on Windows its `PATHEXT` spellings, as `shutil.which`)."""
    if _sys.names_a_file(command):
        return command
    path = _sys.child_path(env)
    if path is None:
        raise NoPath(command)
    for d in path.split(os.pathsep):
        for name in _sys.exe_names(command):
            # POSIX: an empty entry is the working directory — the CHILD's.
            full = os.path.abspath(os.path.join(cwd, d or ".", name))
            if _sys.is_executable(full):
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
    return await _sys.spawn_argv(argv, resolve(argv[0], cwd, env), cwd, env, pass_fd)


async def spawn_pty(argv: list[str], cwd: str, env: dict, pass_fd: int, term: Pty):
    """Start ARGV on the terminal `term`: its stdin/stdout/stderr are the terminal,
    and the child leads a new session with it as its controlling terminal. Same
    gate, same env rule, same control fd as `spawn_argv` — a terminal is a job, not
    a second spawner. `term`'s child side is the job's from here on (the kernel's
    copy goes, so the kernel sees EOF when the job's last copy closes)."""
    return await _sys.spawn_pty(
        argv, resolve(argv[0], cwd, env), cwd, env, pass_fd, term
    )
