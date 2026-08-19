"""proc — THE single process-spawn surface for the whole kernel.

Every `create_subprocess_exec` for an authored program goes through here, exactly as
every `open()` goes through `file_bridge.fs`. That is what makes the ALLOWLIST
unbypassable: there is no second way to start a process, so there is no way around the
check. `fs` earns the running-dir law by being the only opener; `proc` earns the
allowlist by being the only spawner.

## The allowlist is not a runtime toggle

`allowed(rec)` reads the leg's `interpreters` list and nothing else. There is no
per-call override, no `allow_any=True`, and no "try the allowlist, else fall back to
whatever is on PATH". Compare `fs.resolve(..., external=True)`: `external` is a
CALL-SITE argument that a `file_bridge` agent deliberately never forwards from its
record, because the moment it becomes a record field an `update_agent` can delete the
law. Same reasoning here — a per-call escape hatch is a fallback, and a fallback that
can grant arbitrary local code execution is the one this kernel least wants.

## What this does NOT claim

An allowlisted interpreter is a *named* interpreter, not a sandbox. A python on the list
still has `open()`, `socket` and `subprocess`. What the allowlist buys is that the
operator chose WHICH runtime and WHICH environment (a pre-created venv with a known
dependency set) rather than the kernel guessing, and that a program cannot pick a
different one. Anything stronger needs an OS mechanism — `sandbox-exec`, a separate uid,
a container — and none of it is reachable in-process. Stated plainly because the
previous executor's security section was a documented hole, and replacing an honest hole
with a dishonest reassurance would be worse.

## No default interpreter

There is none, deliberately. `python_runtime` could fall back to `sys.executable`
because the host IS python — a FACT about the running process, not a guess. This bridge
spawns whatever the operator listed, so "no interpreter named" has no honest answer and
refuses instead. "By default no defaults."
"""

from __future__ import annotations

import asyncio
import os
from pathlib import Path


class InterpreterRefused(Exception):
    """A requested interpreter is not on this leg's allowlist, or none was requested.

    Its own type because three callers need it and need it differently: `start` turns it
    into the job's `{"error": …}`, `validate_meta` turns it into a WRITE-TIME refusal
    (so the mistake lands next to the typo, not three calls later), and `reflect`
    reports the unresolved state instead of naming an interpreter that would never run.
    """


def allowed(rec: dict) -> list[str]:
    """This leg's allowlisted interpreters, `~` expanded. Empty ⇒ the leg spawns nothing.

    Mirrors `file_bridge`'s `_hidden(rec)` shape: a plain record list, no registry, no
    inheritance. An empty list is a legal and meaningful state — a leg that is wired but
    has been given nothing to run, which refuses every `start` and says so.
    """
    raw = rec.get("interpreters") or []
    if isinstance(raw, str):  # one interpreter written bare, not in a list
        raw = [raw]
    return [str(Path(str(p)).expanduser()) for p in raw if str(p).strip()]


def resolve(rec: dict, requested: str | None) -> str:
    """The interpreter to spawn, or refuse NAMING the legal set.

    `requested` is matched against the allowlist by resolved path, so `~/x/bin/python`
    and its expansion are the same entry. A request for something absent is refused with
    the list in the message — that is the mechanism measured to let a small model
    recover: on 2026-08-06 gemma-4-12B tried `ingress_rule="deny_all"`, the write-time
    refusal named the legal values, and it used `deny_inbound` on its very next turn. A
    refusal that does not say what WOULD work only teaches that something is wrong.

    Requesting nothing is legal when the leg lists exactly one interpreter — there is no
    ambiguity to resolve, so making the caller restate it would be ceremony. With two or
    more, silence is ambiguous and must be refused rather than guessed.
    """
    ok = allowed(rec)
    if not ok:
        raise InterpreterRefused(
            "no interpreters are allowlisted on this leg — an operator must set "
            "`interpreters=[<abs path to an interpreter>]` before a program can run"
        )
    if requested is None or not str(requested).strip():
        if len(ok) == 1:
            return ok[0]
        raise InterpreterRefused(
            f"this leg allows {len(ok)} interpreters, so name one: interpreter=<one of "
            f"{ok}>"
        )
    want = str(Path(str(requested)).expanduser())
    if want not in ok:
        raise InterpreterRefused(f"interpreter {requested!r} is not allowlisted — {ok}")
    return want


def argv(rec: dict, interp: str, program: str) -> list[str]:
    """The full argv. `argv_template` on the record, `<interp>`/`<program>` substituted.

    A template rather than hardcoded flags, because `-u -c` is CPython-specific: `-u` is
    what makes output line-live (and therefore what makes `progress` events useful), and
    node wants `-e`, ruby wants `-e` plus a `$stdout.sync` preamble. The bridge does not
    know which language it is spawning and must not pretend to — so the shape is data.

    Default is the CPython one, because it is the only interpreter family this bundle
    ships a shim for; a leg listing a node binary is expected to carry its own template.
    """
    tmpl = rec.get("argv_template") or ["<interp>", "-u", "-c", "<program>"]
    if isinstance(tmpl, str):
        tmpl = [tmpl]
    return [
        interp if a == "<interp>" else (program if a == "<program>" else str(a))
        for a in tmpl
    ]


async def spawn(
    rec: dict,
    *,
    interpreter: str | None,
    program: str,
    cwd: str | None,
    env_extra: dict,
    pass_fd: int,
):
    """Start the process. Refuses BEFORE spawning if the interpreter is not allowlisted.

    The child inherits `os.environ` plus `env_extra`, and `pass_fd` stays open across the
    exec (`pass_fds` defeats the implied close-on-exec) so the child can speak the
    control wire. The caller closes its own copy of that fd immediately afterwards —
    without which EOF never fires and a parked program never learns the kernel is gone.
    """
    interp = resolve(rec, interpreter)
    return await asyncio.create_subprocess_exec(
        *argv(rec, interp, program),
        cwd=cwd if (isinstance(cwd, str) and cwd) else None,
        stdout=asyncio.subprocess.PIPE,
        stderr=asyncio.subprocess.PIPE,
        env={**os.environ, **env_extra},
        pass_fds=(pass_fd,),
    )
