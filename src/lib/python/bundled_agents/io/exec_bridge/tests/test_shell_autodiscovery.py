"""A PTY `start` with no `command` runs the CHILD's own `$SHELL`.

The shell is AUTODISCOVERED from the environment the record passes down
(`inherit_env`/`env`) — never from the kernel's env unless inherited, never from
a fallback chain. The allowlist still decides: the record's `shell` field
(`true` → `[$SHELL]`, `"login"` → `[$SHELL, "-l"]`) permits that exact line and
nothing wider; every other argv, and every pipe job, still needs `allowed`.

The "shell" here is a tiny executable script, so the tests read the same on
every OS: it prints its argv and echoes one typed line back.
"""

from __future__ import annotations

import asyncio
import os
import stat
import sys

import pytest

from exec_bridge import tools as ebt

PY = sys.executable
PTY = {"cols": 80, "rows": 24}


@pytest.fixture
def fake_shell(tmp_path):
    """An executable that stands in for a login shell: it announces its argv,
    then echoes one typed line — enough to prove WHICH line was spawned and
    that it sits on the terminal."""
    path = tmp_path / "fakesh"
    path.write_text('#!/bin/sh\necho "SHELL_UP $*"\nread line\necho "GOT $line"\n')
    path.chmod(path.stat().st_mode | stat.S_IXUSR | stat.S_IXGRP | stat.S_IXOTH)
    return str(path)


async def _mk(kernel, agent_id, **extra):
    meta = {
        "type": "create_agent",
        "parent_id": kernel.id,
        "id": agent_id,
        "handler_module": "exec_bridge.tools",
        "ingress_rule": "allow_all_ingress",
        "cwd": ".",
        **extra,
    }
    return (await kernel.send([{"target": kernel.id, "payload": meta}]))[0]["result"]


async def _call(kernel, target, payload):
    return (await kernel.send([{"target": target, "payload": payload}]))[0]["result"]


@pytest.fixture
async def k(seeded_kernel):
    seeded_kernel.ctx.ensure_inbox("viewer")
    yield seeded_kernel
    for j in list(ebt._jobs.values()):
        ebt._teardown(j)


async def _events_until(k, pred, secs=10.0):
    """Drain the viewer's inbox until `pred(output_text)` holds; returns the
    accumulated output and every event seen on the way."""
    q = k.ctx.inboxes["viewer"]
    text, events = "", []
    loop = asyncio.get_running_loop()
    end = loop.time() + secs
    while not pred(text):
        left = end - loop.time()
        assert left > 0, f"timed out; got {text!r}"
        ev = await asyncio.wait_for(q.get(), timeout=left)
        events.append(ev)
        if ev.get("type") == "output":
            text += ev["data"]
    return text, events


async def test_a_pty_start_without_command_runs_the_childs_shell(k, fake_shell):
    await _mk(k, "sh", env={"SHELL": fake_shell}, shell=True)
    k.watch("sh", "viewer")
    r = await _call(k, "sh", {"type": "start", "pty": PTY})
    assert r.get("status") == "running" and r.get("pty") is True, r
    jid = r["job_id"]
    text, events = await _events_until(k, lambda t: "SHELL_UP" in t)
    # The announcement names the RESOLVED shell, so a client that let the
    # kernel pick it can show it and re-issue it.
    started = next(e for e in events if e.get("type") == "job_started")
    assert started == {
        "type": "job_started",
        "job_id": jid,
        "command": fake_shell,
        "args": [],
    }, started
    snap = (await _call(k, "sh", {"type": "status", "job_id": jid}))["jobs"][0]
    assert (snap["command"], snap["args"], snap["pty"]) == (fake_shell, [], True)
    # It is a real terminal job: a typed line reaches the shell.
    await _call(k, "sh", {"type": "write", "job_id": jid, "data": "hi\n"})
    await _events_until(k, lambda t: "GOT hi" in t)


async def test_the_shell_comes_from_the_childs_env_not_the_kernels(
    k, fake_shell, monkeypatch
):
    # The kernel HAS a SHELL — but the record passes nothing down, so the child
    # has none, and the start is refused naming where it would come from.
    monkeypatch.setenv("SHELL", fake_shell)
    await _mk(k, "sh", shell=True)
    r = await _call(k, "sh", {"type": "start", "pty": PTY})
    assert (r["reason"], r["arg"]) == ("missing_arg", "SHELL"), r
    assert "inherit_env" in r["hint"], r
    # Passing it down by NAME is the whole repair.
    await _call(k, k.id, {"type": "update_agent", "id": "sh", "inherit_env": ["SHELL"]})
    r = await _call(k, "sh", {"type": "start", "pty": PTY})
    assert r.get("status") == "running", r


async def test_an_empty_shell_in_the_child_env_is_refused_too(k):
    await _mk(k, "sh", env={"SHELL": ""}, shell=True)
    r = await _call(k, "sh", {"type": "start", "pty": PTY})
    assert (r["reason"], r["arg"]) == ("missing_arg", "SHELL"), r


async def test_a_pipe_job_always_names_its_command(k, fake_shell):
    # Autodiscovery is PTY-only: a shell on a pipe has no terminal to talk to.
    await _mk(k, "sh", env={"SHELL": fake_shell}, shell=True)
    r = await _call(k, "sh", {"type": "start"})
    assert (r["reason"], r["arg"]) == ("missing_arg", "command"), r
    # …and an explicit empty command on a PTY is a typo, not "use the shell".
    r = await _call(k, "sh", {"type": "start", "command": "", "pty": PTY})
    assert (r["reason"], r["arg"]) == ("missing_arg", "command"), r


async def test_shell_true_permits_only_the_bare_shell_line(k, fake_shell):
    await _mk(k, "sh", env={"SHELL": fake_shell}, shell=True)
    # extra args are a different line — `shell` does not cover it
    r = await _call(k, "sh", {"type": "start", "args": ["-c", "id"], "pty": PTY})
    assert r["reason"] == "unauthorized", r
    assert "permits only the PTY line" in r["error"], r
    # the same shell on a PIPE is not the permitted line either
    r = await _call(k, "sh", {"type": "start", "command": fake_shell})
    assert r["reason"] == "unauthorized", r
    # naming the shell explicitly on a PTY is the same line, so it is permitted
    r = await _call(k, "sh", {"type": "start", "command": fake_shell, "pty": PTY})
    assert r.get("status") == "running", r


async def test_shell_login_permits_exactly_the_login_line(k, fake_shell):
    await _mk(k, "sh", env={"SHELL": fake_shell}, shell="login")
    k.watch("sh", "viewer")
    # the bare shell is NOT the login line
    r = await _call(k, "sh", {"type": "start", "pty": PTY})
    assert r["reason"] == "unauthorized", r
    assert "-l" in r["error"], r
    r = await _call(k, "sh", {"type": "start", "args": ["-l"], "pty": PTY})
    assert r.get("status") == "running", r
    await _events_until(k, lambda t: "SHELL_UP -l" in t)


async def test_without_shell_the_resolved_line_still_needs_allowed(k, fake_shell):
    # No `shell` on the record: the autodiscovered line is judged by `allowed`
    # like any other — autodiscovery never widens the gate.
    await _mk(k, "sh", env={"SHELL": fake_shell})
    r = await _call(k, "sh", {"type": "start", "pty": PTY})
    assert r["reason"] == "unauthorized", r
    await _call(
        k,
        k.id,
        {"type": "update_agent", "id": "sh", "allowed": [fake_shell]},
    )
    r = await _call(k, "sh", {"type": "start", "pty": PTY})
    assert r.get("status") == "running", r


async def test_shell_does_not_disable_the_allowlist(k, fake_shell):
    await _mk(k, "sh", env={"SHELL": fake_shell}, shell=True, allowed=[rf"{PY} -c .*"])
    r = await _call(k, "sh", {"type": "start", "command": PY, "args": ["-c", "0"]})
    assert r.get("status") == "running", r


async def test_shell_is_refused_at_write_unless_true_false_or_login(k):
    for bad in ("bash", 1, "yes", ["login"]):
        r = await _mk(k, f"bad_{os.urandom(3).hex()}", shell=bad)
        assert r.get("reason") == "invalid_meta", (bad, r)
        assert "legal values" in r["error"], r
    for ok in (True, False, "login"):
        r = await _mk(k, f"ok_{os.urandom(3).hex()}", shell=ok)
        assert r.get("id"), (ok, r)


async def test_reflect_advertises_the_shell_permission(k):
    await _mk(k, "plain")
    await _mk(k, "login", shell="login")
    assert (await _call(k, "plain", {"type": "reflect"}))["shell"] is False
    assert (await _call(k, "login", {"type": "reflect"}))["shell"] == "login"


async def test_write_bytes_is_verbatim_so_enter_reaches_the_shell(k, fake_shell):
    """A `<call>` strips a scalar like `data`, so a key ending in Enter rides
    `bytes`, written verbatim (owner 2026-10-04): the line runs. Exactly one of
    `data`/`bytes` is accepted."""
    await _mk(k, "sh", env={"SHELL": fake_shell}, shell=True)
    k.watch("sh", "viewer")
    r = await _call(k, "sh", {"type": "start", "pty": PTY})
    jid = r["job_id"]
    await _events_until(k, lambda t: "SHELL_UP" in t)
    both = await _call(
        k, "sh", {"type": "write", "job_id": jid, "data": "a", "bytes": b"b"}
    )
    assert both.get("arg") == "data" and "exactly one" in both["error"], both
    none = await _call(k, "sh", {"type": "write", "job_id": jid})
    assert none.get("arg") == "data", none
    out = await _call(k, "sh", {"type": "write", "job_id": jid, "bytes": b"yo\n"})
    assert out == {"written": 3}, out
    await _events_until(k, lambda t: "GOT yo" in t)
