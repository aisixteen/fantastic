"""python_runtime — async Python JOB spawner: start/status/stop + progress events."""

from __future__ import annotations

import asyncio
import secrets
import time


async def _make(kernel, **meta):
    rec = await kernel.send(
        "fs_loader",
        {"type": "create_agent", "handler_module": "python_runtime.tools", **meta},
    )
    return rec["id"]


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


async def _poll_done(kernel, pid: str, job_id: str, timeout: float = 10.0):
    end = time.monotonic() + timeout
    while time.monotonic() < end:
        r = await kernel.send(pid, {"type": "status", "job_id": job_id})
        j = r["jobs"][0]
        if j["status"] in ("done", "failed", "stopped"):
            return j
        await asyncio.sleep(0.05)
    raise AssertionError("job did not finish in time")


# ─── verb surface ───────────────────────────────────────────────


async def test_reflect_lists_verbs(seeded_kernel):
    pid = await _make(seeded_kernel)
    r = await seeded_kernel.send(pid, {"type": "reflect"})
    assert r["id"] == pid
    for v in ("start", "status", "stop", "interrupt", "clear", "reflect", "boot"):
        assert v in r["verbs"]
    assert r["running"] == 0
    assert "job_done" in r["emits"]


async def test_unknown_verb_errors(seeded_kernel):
    pid = await _make(seeded_kernel)
    r = await seeded_kernel.send(pid, {"type": "garbage"})
    assert "error" in r


# ─── start: non-blocking + parallel ─────────────────────────────


async def test_start_is_nonblocking_and_parallel(seeded_kernel):
    """3 sleeper jobs start near-instantly (kernel not blocked) and are all
    in-flight at once (parallel)."""
    pid = await _make(seeded_kernel)
    t0 = time.monotonic()
    jids = []
    for n in range(3):
        r = await seeded_kernel.send(
            pid,
            {"type": "start", "code": f"import time; time.sleep(0.6); print({n})"},
        )
        assert r["status"] == "running" and r["job_id"]
        jids.append(r["job_id"])
    started = time.monotonic() - t0
    assert started < 0.5, f"start blocked ({started:.2f}s) — should return at once"
    # all three alive simultaneously → parallel
    st = await seeded_kernel.send(pid, {"type": "status"})
    running = [j for j in st["jobs"] if j["status"] == "running"]
    assert len(running) == 3
    # and each completes with its own output
    outs = set()
    for jid in jids:
        j = await _poll_done(seeded_kernel, pid, jid)
        assert j["status"] == "done" and j["exit_code"] == 0
        outs.add(j["stdout"].strip())
    assert outs == {"0", "1", "2"}


# ─── progress events + done ─────────────────────────────────────


async def test_progress_events_and_done(seeded_kernel):
    pid = await _make(seeded_kernel)
    q = _watch(seeded_kernel, pid)
    r = await seeded_kernel.send(
        pid, {"type": "start", "code": "for i in range(3): print(i)"}
    )
    evs = await _until_done(q, r["job_id"])
    progress = [
        e["line"] for e in evs if e["type"] == "progress" and e["stream"] == "stdout"
    ]
    assert progress == ["0", "1", "2"]
    done = evs[-1]
    assert done["type"] == "job_done" and done["ok"] is True
    assert done["exit_code"] == 0 and done["stdout"] == "0\n1\n2\n"


async def test_status_carries_full_output(seeded_kernel):
    pid = await _make(seeded_kernel)
    r = await seeded_kernel.send(pid, {"type": "start", "code": "print(2*21)"})
    j = await _poll_done(seeded_kernel, pid, r["job_id"])
    assert "42" in j["stdout"] and j["exit_code"] == 0 and j["status"] == "done"


async def test_stderr_separate_and_nonzero_exit(seeded_kernel):
    pid = await _make(seeded_kernel)
    r = await seeded_kernel.send(
        pid,
        {"type": "start", "code": 'import sys\nsys.stderr.write("oops")\nsys.exit(7)'},
    )
    j = await _poll_done(seeded_kernel, pid, r["job_id"])
    assert "oops" in j["stderr"]
    assert j["exit_code"] == 7 and j["status"] == "failed"


# ─── stop / interrupt ───────────────────────────────────────────


async def test_stop_kills_a_running_job(seeded_kernel):
    pid = await _make(seeded_kernel)
    r = await seeded_kernel.send(
        pid, {"type": "start", "code": "import time\ntime.sleep(30)"}
    )
    jid = r["job_id"]
    k = await seeded_kernel.send(pid, {"type": "stop", "job_id": jid})
    assert k["killed"] >= 1
    j = await _poll_done(seeded_kernel, pid, jid)
    assert j["status"] == "stopped" and j["exit_code"] != 0


async def test_interrupt_running_jobs(seeded_kernel):
    pid = await _make(seeded_kernel)
    r = await seeded_kernel.send(
        pid, {"type": "start", "code": "import time\ntime.sleep(30)"}
    )
    n = await seeded_kernel.send(pid, {"type": "interrupt"})
    assert n["interrupted"] >= 1
    j = await _poll_done(seeded_kernel, pid, r["job_id"])
    assert j["exit_code"] != 0


async def test_clear_drops_finished_jobs(seeded_kernel):
    pid = await _make(seeded_kernel)
    r = await seeded_kernel.send(pid, {"type": "start", "code": "print(1)"})
    await _poll_done(seeded_kernel, pid, r["job_id"])
    c = await seeded_kernel.send(pid, {"type": "clear"})
    assert c["cleared"] >= 1
    st = await seeded_kernel.send(pid, {"type": "status"})
    assert st["jobs"] == []


# ─── cwd + interpreter ──────────────────────────────────────────


async def test_cwd_from_payload(seeded_kernel, tmp_path):
    pid = await _make(seeded_kernel)
    sub = tmp_path / "work"
    sub.mkdir()
    r = await seeded_kernel.send(
        pid,
        {"type": "start", "code": "import os; print(os.getcwd())", "cwd": str(sub)},
    )
    j = await _poll_done(seeded_kernel, pid, r["job_id"])
    assert str(sub) in j["stdout"]


async def test_cwd_from_agent_record(seeded_kernel, tmp_path):
    sub = tmp_path / "agentcwd"
    sub.mkdir()
    pid = await _make(seeded_kernel, cwd=str(sub))
    r = await seeded_kernel.send(
        pid, {"type": "start", "code": "import os; print(os.getcwd())"}
    )
    j = await _poll_done(seeded_kernel, pid, r["job_id"])
    assert str(sub) in j["stdout"]


async def test_start_rejects_empty_code(seeded_kernel):
    pid = await _make(seeded_kernel)
    r = await seeded_kernel.send(pid, {"type": "start", "code": ""})
    assert "error" in r and "code" in r["error"]


async def test_start_rejects_non_string_code(seeded_kernel):
    pid = await _make(seeded_kernel)
    r = await seeded_kernel.send(pid, {"type": "start", "code": 42})
    assert "error" in r


async def test_uses_record_python_field(seeded_kernel, tmp_path):
    """Agent record's `python` selects the interpreter (sentinel proves which ran)."""
    fake = tmp_path / "fake_python"
    fake.write_text("#!/bin/bash\necho FAKE_INTERPRETER_RAN $@\n")
    fake.chmod(0o755)
    pid = await _make(seeded_kernel, python=str(fake))
    r = await seeded_kernel.send(pid, {"type": "start", "code": "anything"})
    j = await _poll_done(seeded_kernel, pid, r["job_id"])
    assert "FAKE_INTERPRETER_RAN" in j["stdout"]


async def test_reflect_includes_resolved_python(seeded_kernel):
    import sys

    pid = await _make(seeded_kernel)
    r = await seeded_kernel.send(pid, {"type": "reflect"})
    assert r["python"] == sys.executable


# ─── _resolve_python (pure, unchanged behavior) ─────────────────


async def test_resolve_python_falls_back_to_sys_executable(seeded_kernel):
    import sys

    from python_runtime.tools import _resolve_python

    assert _resolve_python({}) == sys.executable
    assert _resolve_python({"cwd": "/tmp"}) == sys.executable


async def test_resolve_python_record_python_overrides(seeded_kernel):
    from python_runtime.tools import _resolve_python

    rec = {"python": "/opt/homebrew/bin/python3"}
    assert _resolve_python(rec) == "/opt/homebrew/bin/python3"


async def test_resolve_python_record_venv_resolves_bin(seeded_kernel, tmp_path):
    from python_runtime.tools import _resolve_python

    venv = tmp_path / "demo_venv"
    (venv / "bin").mkdir(parents=True)
    py = venv / "bin" / "python"
    py.write_text("#!/bin/bash\nexit 0\n")
    py.chmod(0o755)
    assert _resolve_python({"venv": str(venv)}) == str(py)


async def test_resolve_python_payload_overrides_record(seeded_kernel):
    from python_runtime.tools import _resolve_python

    rec = {"python": "/from/record"}
    payload = {"python": "/from/payload"}
    assert _resolve_python(rec, payload) == "/from/payload"


async def test_resolve_python_missing_venv_silently_falls_through(seeded_kernel):
    import sys

    from python_runtime.tools import _resolve_python

    assert _resolve_python({"venv": "/no/such/place"}) == sys.executable


# ─── connector: a job reaches the kernel (child → spawner → kernel) ──


async def _wait_line(kernel, pid, jid, needle, timeout=8.0):
    """Poll status until the job's last_line/stdout contains needle (output is
    captured as last_line incrementally; full stdout only lands at job exit)."""
    end = time.monotonic() + timeout
    while time.monotonic() < end:
        r = await kernel.send(pid, {"type": "status", "job_id": jid})
        j = r["jobs"][0]
        if needle in (j.get("last_line") or "") or needle in (j.get("stdout") or ""):
            return j
        await asyncio.sleep(0.05)
    raise AssertionError(f"line {needle!r} not seen in time")


async def test_connector_send_reaches_kernel(seeded_kernel):
    """A job's kernel.send reaches the real kernel and gets a reply back."""
    pid = await _make(seeded_kernel)
    root = (await seeded_kernel.send("kernel", {"type": "reflect", "tree": "none"}))[
        "id"
    ]
    r = await seeded_kernel.send(
        pid, {"type": "start", "code": 'print(kernel.reflect("kernel")["id"])'}
    )
    j = await _poll_done(seeded_kernel, pid, r["job_id"])
    assert j["status"] == "done" and root in j["stdout"]


async def test_connector_send_reaches_another_agent(seeded_kernel):
    """A job reaches ANOTHER agent by id (the read-anything-by-id goal)."""
    pid = await _make(seeded_kernel)
    other = await _make(seeded_kernel)
    code = f'print(kernel.send({other!r}, {{"type":"reflect"}})["id"])'
    r = await seeded_kernel.send(pid, {"type": "start", "code": code})
    j = await _poll_done(seeded_kernel, pid, r["job_id"])
    assert other in j["stdout"]


async def test_connector_emit_from_job(seeded_kernel):
    """A job's kernel.emit fans out to watchers of the target."""
    pid = await _make(seeded_kernel)
    q = _watch(seeded_kernel, pid)  # watch the runtime agent itself
    code = f'kernel.emit({pid!r}, {{"type":"from_job","v":7}})'
    r = await seeded_kernel.send(pid, {"type": "start", "code": code})
    await _poll_done(seeded_kernel, pid, r["job_id"])
    found = False
    end = time.monotonic() + 5
    while time.monotonic() < end:
        ev = await asyncio.wait_for(q.get(), timeout=max(0.05, end - time.monotonic()))
        if ev.get("type") == "from_job" and ev.get("v") == 7:
            found = True
            break
    assert found, "emitted event not received by watcher"


async def test_connector_watch_in_job(seeded_kernel):
    """PUSH parity: a job watches another agent and receives its events live."""
    pid = await _make(seeded_kernel)
    tgt = await _make(seeded_kernel)
    code = (
        "import time, json\n"
        "got = []\n"
        f"off = kernel.watch({tgt!r}, lambda p: got.append(p))\n"
        'kernel.reflect("kernel")  # FIFO round-trip: the watch above is\n'
        "                          # registered upstream before this returns\n"
        'print("READY", flush=True)\n'
        "time.sleep(1.2)\n"
        'print("GOT", json.dumps(got))\n'
    )
    r = await seeded_kernel.send(pid, {"type": "start", "code": code})
    jid = r["job_id"]
    await _wait_line(seeded_kernel, pid, jid, "READY")
    await seeded_kernel.emit(tgt, {"type": "ping", "v": 99})
    j = await _poll_done(seeded_kernel, pid, jid)
    assert '"v": 99' in j["stdout"] or '"v":99' in j["stdout"]


async def test_connector_on_message_in_job(seeded_kernel):
    """PUSH parity: a job receives messages on its owner agent's inbox."""
    pid = await _make(seeded_kernel)
    code = (
        "import time, json\n"
        "got = []\n"
        "off = kernel.on_message(lambda p: got.append(p))\n"
        'kernel.reflect("kernel")  # FIFO round-trip: onmessage registered upstream\n'
        'print("READY", flush=True)\n'
        "time.sleep(1.2)\n"
        'print("GOT", json.dumps(got))\n'
    )
    r = await seeded_kernel.send(pid, {"type": "start", "code": code})
    jid = r["job_id"]
    await _wait_line(seeded_kernel, pid, jid, "READY")
    await seeded_kernel.emit(pid, {"type": "hello", "v": 123})
    j = await _poll_done(seeded_kernel, pid, jid)
    assert "123" in j["stdout"]
