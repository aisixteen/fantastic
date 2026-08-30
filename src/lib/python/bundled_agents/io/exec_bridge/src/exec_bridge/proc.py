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
import os


async def spawn_argv(
    argv: list[str],
    cwd: str | None,
    env_extra: dict,
    pass_fd: int,
):
    """Start ARGV as given. The GATE is the caller's regex allowlist on the full
    command line (`tools._command_permitted`) — this function adds nothing to it.
    The child inherits `os.environ` plus `env_extra`, and `pass_fd` stays open across
    the exec so the child can speak the control wire (a command that does not know the
    wire simply never reads the fd)."""
    return await asyncio.create_subprocess_exec(
        *argv,
        cwd=cwd if (isinstance(cwd, str) and cwd) else None,
        stdout=asyncio.subprocess.PIPE,
        stderr=asyncio.subprocess.PIPE,
        env={**os.environ, **env_extra},
        pass_fds=(pass_fd,),
    )
