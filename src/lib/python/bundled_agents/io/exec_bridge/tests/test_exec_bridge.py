"""exec_bridge — the verb surface, the output pump, and per-call `args`.

All of it language-blind by design: the job table, the two streams, the arguments that
reach a program. Retargeted from the predecessor executor's suite, because none of that
machinery ever knew a language — the interpreter is now a FIELD, wired by the operator
onto the record. (The durable record, the inbox and the task table are in
`test_exec_bridge_jobs.py`; the gate and the allowlist in `test_exec_bridge_gate.py`.)
"""

from __future__ import annotations

import asyncio
import json
import os
import secrets
import sys
import time

import pytest
from exec_bridge import tools as xb


def _real_python() -> str:
    """A real interpreter for the TEST to wire onto a record's allowlist.

    `sys.executable` is the operator's choice here — which is exactly what the
    allowlist models. Skip cleanly rather than assert about a machine that has none."""
    if not (sys.executable and os.access(sys.executable, os.X_OK)):
        pytest.skip("no executable interpreter available to allowlist")
    return sys.executable


async def _make(kernel, **meta) -> str:
    rec = (
        await kernel.send(
            [
                {
                    "target": "kernel",
                    "payload": {
                        "type": "create_agent",
                        "parent_id": kernel.id,
                        "handler_module": "exec_bridge.tools",
                        **meta,
                    },
                }
            ]
        )
    )[0]["result"]
    assert "id" in rec, f"exec_bridge creation failed: {rec!r}"
    return rec["id"]


async def _wired(kernel, **meta) -> str:
    """An OPEN leg allowlisting THIS interpreter. `shim` stays the caller's choice —
    the WIRE is the contract; the shim is a convenience over it."""
    return await _make(
        kernel, ingress_rule="allow_all_ingress", interpreters=[_real_python()], **meta
    )


def _watch(kernel, agent_id: str):
    """Subscribe a synthetic watcher to an agent's inbox; returns its queue."""
    name = "w_" + secrets.token_hex(3)
    kernel.ctx.ensure_inbox(name)
    kernel.watch(agent_id, name)
    return kernel.ctx.inboxes[name]


async def _until_done(q, job_id: str, timeout: float = 10.0):
    """Drain events for one job_id until its job_done; return the list."""
    evs = []
    end = time.monotonic() + timeout
    while True:
        ev = await asyncio.wait_for(q.get(), timeout=max(0.05, end - time.monotonic()))
        if ev.get("job_id") != job_id:
            continue
        evs.append(ev)
        if ev.get("type") == "job_done":
            return evs


async def _poll_done(kernel, xid: str, job_id: str, timeout: float = 10.0):
    end = time.monotonic() + timeout
    while time.monotonic() < end:
        j = (
            (
                await kernel.send(
                    [{"target": xid, "payload": {"type": "status", "job_id": job_id}}]
                )
            )[0]["result"]
        )["jobs"][0]
        if j["status"] in ("done", "failed", "stopped"):
            return j
        await asyncio.sleep(0.05)
    raise AssertionError("job did not finish in time")


# ─── verb surface ───────────────────────────────────────────────


async def test_reflect_advertises_the_verbs_the_allowlist_and_the_wire(seeded_kernel):
    """Without the allowlist and the wire on `reflect`, an authoring model has to GUESS
    an interpreter, and a guess has no legal answer (measured 2026-08-05: opus invented
    a module name because nothing named the real one)."""
    xid = await _wired(seeded_kernel)
    r = (await seeded_kernel.send([{"target": xid, "payload": {"type": "reflect"}}]))[
        0
    ]["result"]
    assert r["id"] == xid
    for v in ("start", "status", "stop", "interrupt", "clear", "reflect", "boot"):
        assert v in r["verbs"]
    assert r["interpreters"] == [_real_python()]
    assert r["shims"] == ["python"]
    assert r["running"] == 0
    assert "job_done" in r["emits"]
    assert "FANTASTIC_CTRL_FD" in r["wire"]
    assert "inbox" in r and "tasks" in r


# ─── start: non-blocking, parallel, language-blind ──────────────


async def test_start_returns_at_once_and_jobs_run_in_parallel(seeded_kernel):
    """`start` must not block the kernel on the program: 3 sleepers start near-instantly
    and are all in flight at once. A blocking spawner would stall every other agent."""
    xid = await _wired(seeded_kernel)
    t0 = time.monotonic()
    jids = []
    for n in range(3):
        r = (
            await seeded_kernel.send(
                [
                    {
                        "target": xid,
                        "payload": {
                            "type": "start",
                            "code": f"import time; time.sleep(0.6); print({n})",
                        },
                    }
                ]
            )
        )[0]["result"]
        assert r["status"] == "running" and r["job_id"], f"start refused: {r!r}"
        jids.append(r["job_id"])
    started = time.monotonic() - t0
    assert started < 0.5, f"start blocked ({started:.2f}s) — should return at once"

    running = [
        j
        for j in (
            (
                await seeded_kernel.send(
                    [{"target": xid, "payload": {"type": "status"}}]
                )
            )[0]["result"]
        )["jobs"]
        if j["status"] == "running"
    ]
    assert len(running) == 3

    outs = set()
    for jid in jids:
        j = await _poll_done(seeded_kernel, xid, jid)
        assert j["status"] == "done" and j["exit_code"] == 0
        outs.add(j["stdout"].strip())
    assert outs == {"0", "1", "2"}


async def test_progress_events_stream_per_line_then_job_done(seeded_kernel):
    """Line-live output is the whole value of the `-u` argv template — without a
    `progress` per line a caller sees nothing until the process exits."""
    xid = await _wired(seeded_kernel)
    q = _watch(seeded_kernel, xid)
    r = (
        await seeded_kernel.send(
            [
                {
                    "target": xid,
                    "payload": {"type": "start", "code": "for i in range(3): print(i)"},
                }
            ]
        )
    )[0]["result"]
    evs = await _until_done(q, r["job_id"])
    progress = [
        e["line"] for e in evs if e["type"] == "progress" and e["stream"] == "stdout"
    ]
    assert progress == ["0", "1", "2"]
    done = evs[-1]
    assert done["type"] == "job_done" and done["ok"] is True
    assert done["exit_code"] == 0 and done["stdout"] == "0\n1\n2\n"


async def test_stderr_stays_separate_and_a_nonzero_exit_reads_failed(seeded_kernel):
    """Merging the streams would make a program's diagnostics indistinguishable from
    its result, and a non-zero exit must not read as `done`."""
    xid = await _wired(seeded_kernel)
    r = (
        await seeded_kernel.send(
            [
                {
                    "target": xid,
                    "payload": {
                        "type": "start",
                        "code": 'import sys\nsys.stderr.write("oops")\nsys.exit(7)',
                    },
                }
            ]
        )
    )[0]["result"]
    j = await _poll_done(seeded_kernel, xid, r["job_id"])
    assert "oops" in j["stderr"] and "oops" not in j["stdout"]
    assert j["exit_code"] == 7 and j["status"] == "failed"


async def test_stop_kills_and_interrupt_signals_a_running_job(seeded_kernel):
    """`stop` (SIGKILL) and `interrupt` (SIGINT) are the two ways a caller ends a job it
    started; a job that survived either would be unreachable and unowned."""
    xid = await _wired(seeded_kernel)
    r = (
        await seeded_kernel.send(
            [
                {
                    "target": xid,
                    "payload": {"type": "start", "code": "import time\ntime.sleep(30)"},
                }
            ]
        )
    )[0]["result"]
    k = (
        await seeded_kernel.send(
            [{"target": xid, "payload": {"type": "stop", "job_id": r["job_id"]}}]
        )
    )[0]["result"]
    assert k["killed"] == 1
    j = await _poll_done(seeded_kernel, xid, r["job_id"])
    assert j["status"] == "stopped" and j["exit_code"] != 0

    r2 = (
        await seeded_kernel.send(
            [
                {
                    "target": xid,
                    "payload": {"type": "start", "code": "import time\ntime.sleep(30)"},
                }
            ]
        )
    )[0]["result"]
    n = (await seeded_kernel.send([{"target": xid, "payload": {"type": "interrupt"}}]))[
        0
    ]["result"]
    assert n["interrupted"] == 1
    j2 = await _poll_done(seeded_kernel, xid, r2["job_id"])
    assert j2["exit_code"] != 0


async def test_clear_forgets_finished_jobs_only(seeded_kernel):
    """`clear` is the one caller that wants the status LABEL, not process liveness — it
    decides what is finished enough to forget, and must not drop a live row."""
    xid = await _wired(seeded_kernel)
    done = (
        await seeded_kernel.send(
            [{"target": xid, "payload": {"type": "start", "code": "print(1)"}}]
        )
    )[0]["result"]
    await _poll_done(seeded_kernel, xid, done["job_id"])
    live = (
        await seeded_kernel.send(
            [
                {
                    "target": xid,
                    "payload": {"type": "start", "code": "import time\ntime.sleep(30)"},
                }
            ]
        )
    )[0]["result"]
    c = (await seeded_kernel.send([{"target": xid, "payload": {"type": "clear"}}]))[0][
        "result"
    ]
    assert c["cleared"] == 1
    jobs = (
        (await seeded_kernel.send([{"target": xid, "payload": {"type": "status"}}]))[0][
            "result"
        ]
    )["jobs"]
    assert [j["job_id"] for j in jobs] == [live["job_id"]]
    (await seeded_kernel.send([{"target": xid, "payload": {"type": "stop"}}]))[0][
        "result"
    ]


async def test_start_refuses_a_missing_or_non_string_code(seeded_kernel):
    """With no code and none on the record there is nothing to run — refuse instead of
    spawning an interpreter with an empty program."""
    xid = await _wired(seeded_kernel)
    for bad in ({"type": "start"}, {"type": "start", "code": ""}):
        r = (await seeded_kernel.send([{"target": xid, "payload": bad}]))[0]["result"]
        assert "error" in r and "code" in r["error"], f"{bad!r} → {r!r}"
        assert "job_id" not in r
    r = (
        await seeded_kernel.send(
            [{"target": xid, "payload": {"type": "start", "code": 42}}]
        )
    )[0]["result"]
    assert "error" in r and "job_id" not in r


async def test_cwd_comes_from_the_payload_or_the_record(seeded_kernel, tmp_path):
    """A tool that runs in the wrong directory reads/writes the wrong files; both the
    per-call and the persisted form must reach the process."""
    xid = await _wired(seeded_kernel)
    sub = tmp_path / "work"
    sub.mkdir()
    r = (
        await seeded_kernel.send(
            [
                {
                    "target": xid,
                    "payload": {
                        "type": "start",
                        "code": "import os; print(os.getcwd())",
                        "cwd": str(sub),
                    },
                }
            ]
        )
    )[0]["result"]
    assert str(sub) in (await _poll_done(seeded_kernel, xid, r["job_id"]))["stdout"]

    onrec = tmp_path / "recwork"
    onrec.mkdir()
    xid2 = await _wired(seeded_kernel, cwd=str(onrec))
    r2 = (
        await seeded_kernel.send(
            [
                {
                    "target": xid2,
                    "payload": {
                        "type": "start",
                        "code": "import os; print(os.getcwd())",
                    },
                }
            ]
        )
    )[0]["result"]
    assert str(onrec) in (await _poll_done(seeded_kernel, xid2, r2["job_id"]))["stdout"]
    assert (
        (await seeded_kernel.send([{"target": xid2, "payload": {"type": "reflect"}}]))[
            0
        ]["result"]
    )["cwd"] == str(onrec)


# ─── per-call arguments (kernel.args) ───────────────────────────


async def test_start_passes_per_call_args_to_the_program(seeded_kernel):
    """Every agent takes its arguments in the payload. Without this the executor is the
    one place the payload is discarded, so a tool can only know what was baked into its
    `code` — it cannot be a pure function, nor be told where to send its result."""
    xid = await _wired(seeded_kernel, shim="python")
    r = (
        await seeded_kernel.send(
            [
                {
                    "target": xid,
                    "payload": {
                        "type": "start",
                        "code": "import json; print(json.dumps(kernel.args))",
                        "args": {"reply_to": "notes", "n": 3, "deep": {"ok": True}},
                    },
                }
            ]
        )
    )[0]["result"]
    j = await _poll_done(seeded_kernel, xid, r["job_id"])
    assert json.loads(j["stdout"].strip()) == {
        "reply_to": "notes",
        "n": 3,
        "deep": {"ok": True},
    }


async def test_args_default_to_empty_when_the_caller_passes_none(seeded_kernel):
    """`{}` — not absent, not `null`: a program may read `kernel.args` unconditionally."""
    xid = await _wired(seeded_kernel, shim="python")
    r = (
        await seeded_kernel.send(
            [
                {
                    "target": xid,
                    "payload": {
                        "type": "start",
                        "code": "import json; print(json.dumps(kernel.args))",
                    },
                }
            ]
        )
    )[0]["result"]
    j = await _poll_done(seeded_kernel, xid, r["job_id"])
    assert json.loads(j["stdout"].strip()) == {}


async def test_unserializable_args_are_refused_loudly(seeded_kernel):
    """They ride the process env as JSON; an object that cannot be encoded must fail
    with its reason BEFORE a process exists, not as a mangled env var."""
    xid = await _wired(seeded_kernel, shim="python")
    r = (
        await seeded_kernel.send(
            [
                {
                    "target": xid,
                    "payload": {
                        "type": "start",
                        "code": "pass",
                        "args": {"f": object()},
                    },
                }
            ]
        )
    )[0]["result"]
    assert "JSON-serializable" in r["error"]
    assert "job_id" not in r


async def test_oversized_args_are_refused_with_the_reason(seeded_kernel):
    """The whole env block caps near 1 MB; past it `execve` fails with a cryptic E2BIG.
    Refuse early and say where bulk data belongs (an agent, passed by id)."""
    xid = await _wired(seeded_kernel, shim="python")
    r = (
        await seeded_kernel.send(
            [
                {
                    "target": xid,
                    "payload": {
                        "type": "start",
                        "code": "pass",
                        "args": {"blob": "x" * (xb.MAX_ARGS_BYTES + 10)},
                    },
                }
            ]
        )
    )[0]["result"]
    assert "too large" in r["error"] and "agent" in r["error"]
    assert "job_id" not in r
