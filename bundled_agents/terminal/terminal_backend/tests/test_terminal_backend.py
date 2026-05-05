"""terminal_backend — PTY shell session."""

from __future__ import annotations

import asyncio
import time

import pytest


async def _make_terminal(kernel, command="/bin/sh"):
    rec = await kernel.send(
        "core",
        {
            "type": "create_agent",
            "handler_module": "terminal_backend.tools",
            "command": command,
        },
    )
    # core auto-boots; give the PTY a moment to spawn
    await asyncio.sleep(0.3)
    return rec["id"]


@pytest.fixture
async def terminal(seeded_kernel):
    tid = await _make_terminal(seeded_kernel)
    yield seeded_kernel, tid
    # cleanup
    await seeded_kernel.send(tid, {"type": "stop"})


async def test_reflect(terminal):
    k, tid = terminal
    r = await k.send(tid, {"type": "reflect"})
    assert r["sentence"].startswith("PTY")
    assert r["running"] is True
    assert r["cols"] >= 40


async def test_shell_done_token_fast_command(terminal):
    k, tid = terminal
    t0 = time.time()
    r = await k.send(tid, {"type": "shell", "cmd": "echo hello-world"})
    elapsed = time.time() - t0
    assert r["completed"] is True
    assert "hello-world" in r["output"]
    # should be much faster than 30s default timeout
    assert elapsed < 5.0


async def test_shell_timeout(terminal):
    k, tid = terminal
    t0 = time.time()
    r = await k.send(tid, {"type": "shell", "cmd": "sleep 60", "timeout": 1.0})
    elapsed = time.time() - t0
    assert r["completed"] is False
    assert r["error"] == "timeout"
    assert elapsed < 3.0  # timeout fired around 1s


async def test_shell_recovers_after_timeout(terminal):
    k, tid = terminal
    await k.send(tid, {"type": "shell", "cmd": "sleep 60", "timeout": 0.5})
    # next shell call should work — Ctrl-C was sent on timeout
    r = await k.send(tid, {"type": "shell", "cmd": "echo recovered"})
    assert r["completed"] is True
    assert "recovered" in r["output"]


async def test_shell_silently_ignores_unknown_args(terminal):
    """Failfast on parameters we explicitly reject is overkill — kwargs
    we don't read are just ignored. `wait` (legacy) goes through cleanly."""
    k, tid = terminal
    r = await k.send(tid, {"type": "shell", "cmd": "echo silently_ok", "wait": 999})
    assert r["completed"] is True
    assert "silently_ok" in r["output"]


async def test_shell_requires_cmd(terminal):
    k, tid = terminal
    r = await k.send(tid, {"type": "shell"})
    assert "error" in r


async def test_write_delivers_bytes(terminal):
    k, tid = terminal
    r = await k.send(tid, {"type": "write", "data": "echo hi-via-write\n"})
    assert r["written"] > 0
    await asyncio.sleep(0.4)
    out = await k.send(tid, {"type": "output"})
    assert "hi-via-write" in out["output"]


async def test_resize(terminal):
    k, tid = terminal
    r = await k.send(tid, {"type": "resize", "cols": 100, "rows": 30})
    assert r["resized"] is True
    refl = await k.send(tid, {"type": "reflect"})
    assert refl["cols"] == 100
    assert refl["rows"] == 30


async def test_shell_without_boot_errors(seeded_kernel):
    rec = await seeded_kernel.send(
        "core",
        {"type": "create_agent", "handler_module": "terminal_backend.tools"},
    )
    # core auto-boots, so manually stop first
    await seeded_kernel.send(rec["id"], {"type": "stop"})
    r = await seeded_kernel.send(rec["id"], {"type": "shell", "cmd": "echo x"})
    assert "error" in r
    assert "not running" in r["error"]


async def test_unknown_verb_errors(terminal):
    k, tid = terminal
    r = await k.send(tid, {"type": "garbage"})
    assert "error" in r


async def test_shutdown_kills_pty_via_delete_agent(seeded_kernel):
    """End-to-end: when core.delete_agent runs, terminal_backend's
    `shutdown` verb must fire and tear down the PTY. Without this hook
    the subprocess outlives its agent record and keeps emitting output
    to a dead inbox (visible as ghost sprites in telemetry views).

    We assert two things:
      1. `_procs` no longer has an entry for the agent id (the
         in-memory state was cleared).
      2. waitpid finds the child reaped or reapable (SIGKILL landed).
         os.kill(pid, 0) is unreliable on macOS — zombies report
         alive until the parent waits — so we use waitpid which
         reaps and tells us the child terminated.
    """
    import asyncio
    import os

    from terminal_backend.tools import _procs

    rec = await seeded_kernel.send(
        "core",
        {"type": "create_agent", "handler_module": "terminal_backend.tools"},
    )
    tid = rec["id"]
    state = _procs.get(tid)
    assert state and state.get("pid"), "boot should spawn a PTY"
    pid = state["pid"]
    r = await seeded_kernel.send("core", {"type": "delete_agent", "id": tid})
    assert r.get("deleted") is True
    assert tid not in _procs, "shutdown must drop the _procs entry"
    # Reap the child. waitpid returns (pid, status) once the child
    # has exited; on macOS this is the reliable liveness probe (kill
    # -0 reports success for un-reaped zombies).
    reaped = (0, 0)
    for _ in range(20):
        try:
            reaped = os.waitpid(pid, os.WNOHANG)
        except ChildProcessError:
            # Already reaped by another path — counts as success.
            reaped = (pid, 0)
            break
        if reaped[0] == pid:
            break
        await asyncio.sleep(0.05)
    assert reaped[0] == pid, f"PTY pid {pid} not reaped after shutdown"
