"""exec_bridge — the general-purpose COMMAND runner.

One suite for the whole surface: the regex gate, the write-time refusals, the
job lifecycle (start/status/stop/clear), the task-table row, the OS reap, and
the control-wire mail path. The predecessor suites (1,800 lines) tested a
deleted design — inline `code`, an interpreter allowlist, shims — and went with
it; this bundle runs COMMANDS, and its one gate is `allowed` (regexes
full-matched against the shell-joined command line).
"""

from __future__ import annotations

import asyncio
import sys


async def _mk(kernel, agent_id="runner", **extra):
    meta = {
        "type": "create_agent",
        "parent_id": kernel.id,
        "id": agent_id,
        "handler_module": "exec_bridge.tools",
        "ingress_rule": "allow_all_ingress",
        **extra,
    }
    r = (await kernel.send([{"target": kernel.id, "payload": meta}]))[0]["result"]
    return r


async def _start(kernel, agent_id, command, args=None, **kw):
    return (
        await kernel.send(
            [
                {
                    "target": agent_id,
                    "payload": {
                        "type": "start",
                        "command": command,
                        "args": args or [],
                        **kw,
                    },
                }
            ]
        )
    )[0]["result"]


async def _wait_done(kernel, agent_id, job_id, secs=5.0):
    """Poll status until the job leaves `running` — event-driven tests poll the
    THING, not a sleep guess."""
    for _ in range(int(secs / 0.05)):
        st = (
            await kernel.send(
                [{"target": agent_id, "payload": {"type": "status", "job_id": job_id}}]
            )
        )[0]["result"]
        row = st["jobs"][0]
        if row["status"] != "running":
            return row
        await asyncio.sleep(0.05)
    raise AssertionError(f"job {job_id} still running after {secs}s")


# ─── the gate ───────────────────────────────────────────────────


async def test_no_allowlist_runs_nothing(seeded_kernel):
    await _mk(seeded_kernel)
    r = await _start(seeded_kernel, "runner", "echo", ["hi"])
    assert "no commands are allowed" in r["error"]


async def test_unmatched_command_refused_naming_the_patterns(seeded_kernel):
    await _mk(seeded_kernel, allowed=[r"echo( .*)?"])
    r = await _start(seeded_kernel, "runner", "cat", ["/etc/passwd"])
    assert "not allowed" in r["error"] and "echo( .*)?" in r["error"]


async def test_match_is_anchored_not_a_prefix(seeded_kernel):
    """`echo.*` must not smuggle a second command past the gate — the line is
    shell-joined and FULL-matched, so the metachar arrives quoted and the
    pattern sees one argv, not a shell string."""
    await _mk(seeded_kernel, allowed=[r"echo [a-z ]*"])
    r = await _start(seeded_kernel, "runner", "echo", ["hi; rm -rf /"])
    assert "not allowed" in r["error"]


async def test_write_time_refusal_for_broken_regex(seeded_kernel):
    r = await _mk(seeded_kernel, agent_id="r2", allowed=["("])
    assert "broken regex" in r["error"]


async def test_write_time_refusal_for_dead_fields(seeded_kernel):
    for field in ("code", "shim", "interpreter", "interpreters", "argv_template"):
        r = await _mk(seeded_kernel, agent_id=f"r_{field}", **{field: "x"})
        assert "is not a field" in r["error"], (field, r)


# ─── the job lifecycle ──────────────────────────────────────────


async def test_command_runs_and_captures_output(seeded_kernel):
    await _mk(seeded_kernel, allowed=[r"echo( .*)?"])
    r = await _start(seeded_kernel, "runner", "echo", ["hello", "world"])
    assert r.get("status") == "running", r
    row = await _wait_done(seeded_kernel, "runner", r["job_id"])
    assert row["exit_code"] == 0
    assert row["last_line"] == "hello world"


async def test_nonzero_exit_is_failed(seeded_kernel):
    await _mk(seeded_kernel, allowed=[r".*"])
    r = await _start(seeded_kernel, "runner", "false")
    row = await _wait_done(seeded_kernel, "runner", r["job_id"])
    assert row["status"] == "failed" and row["exit_code"] != 0


async def test_stop_kills_a_running_job(seeded_kernel):
    await _mk(seeded_kernel, allowed=[r"sleep .*"])
    r = await _start(seeded_kernel, "runner", "sleep", ["30"])
    s = (
        await seeded_kernel.send(
            [{"target": "runner", "payload": {"type": "stop", "job_id": r["job_id"]}}]
        )
    )[0]["result"]
    assert s.get("killed"), s
    row = await _wait_done(seeded_kernel, "runner", r["job_id"])
    assert row["status"] != "running"


async def test_env_reaches_the_child(seeded_kernel):
    await _mk(seeded_kernel, allowed=[r"/bin/sh -c .*"])
    r = await _start(
        seeded_kernel, "runner", "/bin/sh", ["-c", "echo $MARK"], env={"MARK": "x42"}
    )
    row = await _wait_done(seeded_kernel, "runner", r["job_id"])
    assert row["last_line"] == "x42"


async def test_many_jobs_run_in_parallel(seeded_kernel):
    await _mk(seeded_kernel, allowed=[r"echo( .*)?"])
    ids = []
    for i in range(3):
        r = await _start(seeded_kernel, "runner", "echo", [str(i)])
        ids.append(r["job_id"])
    for i, j in enumerate(ids):
        row = await _wait_done(seeded_kernel, "runner", j)
        assert row["exit_code"] == 0 and row["last_line"] == str(i)


# ─── kernel integration ─────────────────────────────────────────


async def test_a_job_holds_a_task_row_and_cancel_reaps_it(seeded_kernel):
    await _mk(seeded_kernel, allowed=[r"sleep .*"])
    r = await _start(seeded_kernel, "runner", "sleep", ["30"])
    st = (
        await seeded_kernel.send(
            [{"target": "runner", "payload": {"type": "status", "job_id": r["job_id"]}}]
        )
    )[0]["result"]
    task_id = st["jobs"][0]["task_id"]
    assert task_id
    c = (
        await seeded_kernel.send(
            [{"target": "kernel", "payload": {"type": "cancel", "task_id": task_id}}]
        )
    )[0]["result"]
    assert c.get("cancelled"), c
    row = await _wait_done(seeded_kernel, "runner", r["job_id"])
    assert row["status"] != "running"


async def test_delete_agent_reaps_running_jobs(seeded_kernel):
    import exec_bridge.tools as ebt

    await _mk(seeded_kernel, allowed=[r"sleep .*"])
    r = await _start(seeded_kernel, "runner", "sleep", ["30"])
    proc = ebt._jobs[r["job_id"]]["proc"]
    await seeded_kernel.send(
        [
            {
                "target": seeded_kernel.id,
                "payload": {"type": "delete_agent", "id": "runner"},
            }
        ]
    )
    for _ in range(60):
        if proc.returncode is not None:
            break
        await asyncio.sleep(0.05)
    assert proc.returncode is not None, "OS subprocess survived delete_agent"


# ─── the control wire ───────────────────────────────────────────

_WIRE_CHILD = (
    "import os,socket,struct,json,sys\n"
    "fd=int(os.environ['FANTASTIC_CTRL_FD'])\n"
    "s=socket.socket(fileno=fd)\n"
    "def frame(o):\n"
    "  b=json.dumps(o).encode(); s.sendall(struct.pack('>I',len(b))+b)\n"
    "frame({'op':'onmessage'})\n"
    "hdr=s.recv(4); n=struct.unpack('>I',hdr)[0]; m=json.loads(s.recv(n))\n"
    "print('got', m['payload']['type']); sys.stdout.flush()\n"
)


async def test_mail_reaches_a_live_job_over_the_wire(seeded_kernel):
    """A command may still speak the control wire — the fd survives the redesign.
    The child here is `python -c <program>`: just a COMMAND with args, which is
    the whole point of the general-purpose runner."""
    py = sys.executable
    await _mk(seeded_kernel, allowed=[rf"{py} -c .*"])
    r = await _start(seeded_kernel, "runner", py, ["-c", _WIRE_CHILD])
    await asyncio.sleep(0.4)  # let it register on_message
    ack = (
        await seeded_kernel.send([{"target": "runner", "payload": {"type": "ping"}}])
    )[0]["result"]
    assert ack == {"queued": True}, ack
    row = await _wait_done(seeded_kernel, "runner", r["job_id"])
    assert "got ping" in (row.get("last_line") or "")


async def test_mail_with_no_live_job_is_a_loud_error(seeded_kernel):
    await _mk(seeded_kernel, allowed=[r"echo( .*)?"])
    r = (await seeded_kernel.send([{"target": "runner", "payload": {"type": "ping"}}]))[
        0
    ]["result"]
    assert "error" in r
