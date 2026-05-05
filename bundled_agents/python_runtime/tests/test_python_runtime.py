"""python_runtime — subprocess Python exec verb behavior."""

from __future__ import annotations

import asyncio
import time


async def _make(kernel, **meta):
    rec = await kernel.send(
        "core",
        {"type": "create_agent", "handler_module": "python_runtime.tools", **meta},
    )
    return rec["id"]


async def test_reflect_lists_verbs(seeded_kernel):
    pid = await _make(seeded_kernel)
    r = await seeded_kernel.send(pid, {"type": "reflect"})
    assert r["id"] == pid
    for v in ("exec", "interrupt", "stop", "reflect", "boot"):
        assert v in r["verbs"]
    assert r["in_flight"] == 0


async def test_exec_print(seeded_kernel):
    pid = await _make(seeded_kernel)
    r = await seeded_kernel.send(pid, {"type": "exec", "code": "print(2*21)"})
    assert "42" in r["stdout"]
    assert r["exit_code"] == 0
    assert r["timed_out"] is False
    assert r["stderr"] == ""


async def test_exec_stderr_and_nonzero_exit(seeded_kernel):
    pid = await _make(seeded_kernel)
    r = await seeded_kernel.send(
        pid,
        {
            "type": "exec",
            "code": 'import sys\nsys.stderr.write("oops")\nsys.exit(7)',
        },
    )
    assert "oops" in r["stderr"]
    assert r["exit_code"] == 7
    assert r["timed_out"] is False


async def test_exec_timeout(seeded_kernel):
    pid = await _make(seeded_kernel)
    t0 = time.monotonic()
    r = await seeded_kernel.send(
        pid,
        {"type": "exec", "code": "import time\ntime.sleep(60)", "timeout": 0.4},
    )
    elapsed = time.monotonic() - t0
    assert r["timed_out"] is True
    assert r["exit_code"] != 0
    assert elapsed < 2.0, f"timeout escaped (took {elapsed:.2f}s)"


async def test_exec_with_cwd(seeded_kernel, tmp_path):
    """Process runs in the given cwd."""
    pid = await _make(seeded_kernel)
    sub = tmp_path / "work"
    sub.mkdir()
    r = await seeded_kernel.send(
        pid,
        {"type": "exec", "code": "import os; print(os.getcwd())", "cwd": str(sub)},
    )
    assert str(sub) in r["stdout"]


async def test_exec_cwd_from_agent_record(seeded_kernel, tmp_path):
    """Agent record's cwd is used when payload doesn't override."""
    sub = tmp_path / "agentcwd"
    sub.mkdir()
    pid = await _make(seeded_kernel, cwd=str(sub))
    r = await seeded_kernel.send(
        pid, {"type": "exec", "code": "import os; print(os.getcwd())"}
    )
    assert str(sub) in r["stdout"]


async def test_exec_rejects_empty_code(seeded_kernel):
    pid = await _make(seeded_kernel)
    r = await seeded_kernel.send(pid, {"type": "exec", "code": ""})
    assert "error" in r and "code" in r["error"]


async def test_exec_rejects_non_string_code(seeded_kernel):
    pid = await _make(seeded_kernel)
    r = await seeded_kernel.send(pid, {"type": "exec", "code": 42})
    assert "error" in r


async def test_interrupt_running_process(seeded_kernel):
    """Send long-running exec; interrupt; assert non-zero exit."""
    pid = await _make(seeded_kernel)
    task = asyncio.create_task(
        seeded_kernel.send(
            pid,
            {"type": "exec", "code": "import time\ntime.sleep(10)", "timeout": 30},
        )
    )
    # Wait for the subprocess to actually start before signaling.
    for _ in range(40):
        await asyncio.sleep(0.05)
        rfl = await seeded_kernel.send(pid, {"type": "reflect"})
        if rfl["in_flight"] >= 1:
            break
    n = await seeded_kernel.send(pid, {"type": "interrupt"})
    assert n["interrupted"] >= 1
    r = await asyncio.wait_for(task, timeout=5.0)
    assert r["exit_code"] != 0


async def test_stop_running_process(seeded_kernel):
    pid = await _make(seeded_kernel)
    task = asyncio.create_task(
        seeded_kernel.send(
            pid,
            {"type": "exec", "code": "import time\ntime.sleep(10)", "timeout": 30},
        )
    )
    for _ in range(40):
        await asyncio.sleep(0.05)
        rfl = await seeded_kernel.send(pid, {"type": "reflect"})
        if rfl["in_flight"] >= 1:
            break
    k = await seeded_kernel.send(pid, {"type": "stop"})
    assert k["killed"] >= 1
    r = await asyncio.wait_for(task, timeout=5.0)
    assert r["exit_code"] != 0


async def test_unknown_verb_errors(seeded_kernel):
    pid = await _make(seeded_kernel)
    r = await seeded_kernel.send(pid, {"type": "garbage"})
    assert "error" in r
