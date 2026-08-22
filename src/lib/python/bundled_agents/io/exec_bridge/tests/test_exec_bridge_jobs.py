"""exec_bridge — the durable record, the addressable inbox, and the task table.

The three ways a job outlives the call that made it: the record's `code` is re-run by
`boot` (and its refusal must PROPAGATE — the silent-success hole this bundle exists to
close), a live program is MAIL-addressable by any verb the bundle does not own, and every
job holds a row in the kernel task table so `ps` sees it and `cancel` reaps the real OS
process. Retargeted from the predecessor executor's suite; the reaping assertions are the
orphan-bug regressions, kept at full strength.
"""

from __future__ import annotations

import asyncio
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
    rec = await kernel.send(
        kernel.id,
        {"type": "create_agent", "handler_module": "exec_bridge.tools", **meta},
    )
    assert "id" in rec, f"exec_bridge creation failed: {rec!r}"
    return rec["id"]


async def _wired(kernel, **meta) -> str:
    """An OPEN leg allowlisting THIS interpreter. `shim` stays the caller's choice."""
    return await _make(
        kernel, ingress_rule="allow_all_ingress", interpreters=[_real_python()], **meta
    )


def _watch(kernel, agent_id: str):
    """Subscribe a synthetic watcher to an agent's inbox; returns its queue."""
    name = "w_" + secrets.token_hex(3)
    kernel.ctx.ensure_inbox(name)
    kernel.watch(agent_id, name)
    return kernel.ctx.inboxes[name]


async def _poll_done(kernel, xid: str, job_id: str, timeout: float = 10.0):
    end = time.monotonic() + timeout
    while time.monotonic() < end:
        j = (await kernel.send(xid, {"type": "status", "job_id": job_id}))["jobs"][0]
        if j["status"] in ("done", "failed", "stopped"):
            return j
        await asyncio.sleep(0.05)
    raise AssertionError("job did not finish in time")


async def _poll_settled(kernel, xid: str, n: int = 1, timeout: float = 10.0):
    """Wait until the leg holds `n` jobs and NONE is live — so `boot` will re-start
    rather than no-op, and stdout has been finalized on every row."""
    end = time.monotonic() + timeout
    while time.monotonic() < end:
        jobs = (await kernel.send(xid, {"type": "status"}))["jobs"]
        if len(jobs) >= n and all(
            j["status"] not in ("running", "stopping") for j in jobs
        ):
            return jobs
        await asyncio.sleep(0.05)
    raise AssertionError("leg never settled")


async def _dead(process, timeout: float = 5.0) -> bool:
    """Poll until the OS process is really gone (reaped, returncode set)."""
    end = time.monotonic() + timeout
    while time.monotonic() < end:
        if process.returncode is not None:
            return True
        await asyncio.sleep(0.05)
    return False


# A durable tool: parked on its owner's inbox forever, echoing what it is mailed.
_ECHO = (
    "import threading\n"
    "def _on(m):\n"
    "    if isinstance(m, dict) and m.get('type') == 'send':\n"
    "        kernel.emit({'type':'say','text':m.get('text'),"
    "'client_id':m.get('client_id')})\n"
    "kernel.on_message(_on)\n"
    "threading.Event().wait()\n"
)


async def _collect_says(q, text: str, window: float = 1.5) -> list:
    """Drain a watcher queue for `window` seconds; return all `say` events for `text`."""
    out = []
    end = time.monotonic() + window
    while time.monotonic() < end:
        try:
            ev = await asyncio.wait_for(
                q.get(), timeout=max(0.05, end - time.monotonic())
            )
        except TimeoutError:
            break
        if ev.get("type") == "say" and ev.get("text") == text:
            out.append(ev)
    return out


# ─── the record's `code` is a durable tool ──────────────────────


async def test_start_with_no_code_falls_back_to_the_record(seeded_kernel):
    """The persisted `code` IS the tool: `start` with no payload code must run it, or a
    durable tool could only ever be invoked by resending its own source."""
    xid = await _wired(seeded_kernel)
    await seeded_kernel.send(
        "kernel_state",
        {"type": "update_agent", "id": xid, "code": "print('FROM_THE_RECORD')"},
    )
    r = await seeded_kernel.send(xid, {"type": "start"})
    assert r["status"] == "running", f"record code not used: {r!r}"
    j = await _poll_done(seeded_kernel, xid, r["job_id"])
    assert "FROM_THE_RECORD" in j["stdout"]


async def test_create_agent_with_code_auto_runs_via_boot(seeded_kernel):
    """`create_agent` boots the record it just made, so authoring a tool and running it
    are ONE call — and the same path revives it after a kernel restart."""
    xid = await _wired(seeded_kernel, id="xb_autoboot", code="print('AUTOBOOTED')")
    jobs = await _poll_settled(seeded_kernel, xid, n=1)
    assert len(jobs) == 1, f"create_agent did not boot the code: {jobs!r}"
    snap = (
        await seeded_kernel.send(xid, {"type": "status", "job_id": jobs[0]["job_id"]})
    )["jobs"][0]
    assert "AUTOBOOTED" in snap["stdout"] and snap["exit_code"] == 0


async def test_boot_is_idempotent_while_a_job_is_live(seeded_kernel):
    """Boot must not stack a second copy of a durable tool that is already parked — two
    harnesses on one inbox double-deliver every inbound send."""
    xid = await _wired(seeded_kernel, id="xb_bootonce", shim="python", code=_ECHO)
    await asyncio.sleep(0.4)  # create → boot → parked on the inbox
    assert await seeded_kernel.send(xid, {"type": "boot"}) is None
    jobs = (await seeded_kernel.send(xid, {"type": "status"}))["jobs"]
    assert len(jobs) == 1, f"boot stacked another job: {jobs!r}"
    await seeded_kernel.send(xid, {"type": "stop"})


# ─── boot PROPAGATES the refusal (the silent-success hole) ──────


async def test_boot_returns_the_spawn_error_when_nothing_is_allowlisted(seeded_kernel):
    """THE hole this bundle exists to close: the predecessor executor's `_boot` discarded the start
    reply, so a record that cannot spawn booted, failed, and still looked created —
    indistinguishable from a program that ran and did nothing. `boot` must RETURN the
    error, not None."""
    xid = await _make(
        seeded_kernel,
        ingress_rule="allow_all_ingress",
        id="xb_boot_noalw",
        code="print('must never run')",
    )
    r = await seeded_kernel.send(xid, {"type": "boot"})
    assert isinstance(r, dict) and "error" in r, f"boot reported silent success: {r!r}"
    assert "no interpreters are allowlisted" in r["error"]
    assert "operator must set" in r["error"]
    assert (await seeded_kernel.send(xid, {"type": "status"}))["jobs"] == []


async def test_boot_returns_the_refusal_after_the_allowlist_is_narrowed(seeded_kernel):
    """The HYDRATION path — the one route by which an inconsistent record still exists.

    `update_agent` can no longer produce this state: `validate_meta` validates the MERGED
    record, so narrowing `interpreters` out from under a named `interpreter` is refused at
    the write (that is the fix for the false-refusal bug of 2026-08-07, and it closed this
    hole as a side effect). But the loader hydrates `agent.json` straight into a record
    WITHOUT the write hook, so a tree persisted before the allowlist changed — or edited
    on disk — can still boot naming an interpreter that is no longer allowed. `boot` is
    the only place left to be honest about it, which is why it must propagate.

    So the fixture mutates the record DIRECTLY (`Agent.update`, the substrate method the
    loader effectively uses) rather than through the verb — reproducing what hydration can
    hand us, not what a caller is allowed to write."""
    py = _real_python()
    xid = await _wired(
        seeded_kernel, id="xb_boot_narrow", interpreter=py, code="print('ran once')"
    )
    first = await _poll_settled(seeded_kernel, xid, n=1)
    assert (
        "ran once"
        in (
            await seeded_kernel.send(
                xid, {"type": "status", "job_id": first[0]["job_id"]}
            )
        )["jobs"][0]["stdout"]
    )

    # The verb WOULD refuse this — assert that, so the guard cannot silently regress…
    refused = await seeded_kernel.send(
        "kernel_state",
        {"type": "update_agent", "id": xid, "interpreters": ["/nowhere/bin/python"]},
    )
    assert refused.get("updated") is False, (
        "narrowing under a named interpreter must refuse"
    )
    assert "not allowlisted" in refused["error"]
    # …then reach the state the way hydration does: straight onto the record.
    seeded_kernel.update(xid, interpreters=["/nowhere/bin/python"])

    r = await seeded_kernel.send(xid, {"type": "boot"})
    assert isinstance(r, dict) and "error" in r, f"boot reported silent success: {r!r}"
    assert "not allowlisted" in r["error"] and "/nowhere/bin/python" in r["error"]
    jobs = (await seeded_kernel.send(xid, {"type": "status"}))["jobs"]
    assert len(jobs) == 1, f"a refused boot must spawn nothing: {jobs!r}"


# ─── the addressable inbox (mail for the running program) ───────


async def test_an_unowned_verb_is_mail_for_a_live_job(seeded_kernel):
    """A running program is addressable: a verb the bundle does not own is MAIL, ACKed
    `{queued:true}`. Only the ACK happens here — the substrate's own dispatch fanout does
    the delivery, so re-emitting would double every message."""
    xid = await _wired(seeded_kernel, id="xb_mail", shim="python", code=_ECHO)
    await asyncio.sleep(0.4)
    assert await seeded_kernel.send(xid, {"type": "tick", "v": 1}) == {"queued": True}
    q = _watch(seeded_kernel, xid)
    await seeded_kernel.send(xid, {"type": "send", "text": "mailed", "client_id": "c"})
    says = await _collect_says(q, "mailed")
    assert len(says) == 1 and says[0]["client_id"] == "c", f"got {says!r}"
    await seeded_kernel.send(xid, {"type": "stop"})


async def test_mail_with_no_live_job_is_a_loud_error(seeded_kernel):
    """A script that only DEFINES something exits before its first turn arrives. Silent
    acceptance would look like delivery to a program that is not there."""
    xid = await _wired(seeded_kernel)
    r = await seeded_kernel.send(xid, {"type": "tick"})
    assert "error" in r and "'tick'" in r["error"]
    assert "no running job" in r["error"]


async def test_a_restart_does_not_double_deliver(seeded_kernel):
    """A durable tool re-started (a repairing model does this constantly) must not leave
    two harnesses both processing the owner inbox — exactly ONE echo. The supersede must
    also not evict its own replacement (the bug `c2a4297` fixed)."""
    xid = await _wired(seeded_kernel, id="xb_restart", shim="python", code=_ECHO)
    await asyncio.sleep(0.4)  # first (boot) job registered on_message
    r = await seeded_kernel.send(xid, {"type": "start"})  # durable restart → 2nd job
    assert r["status"] == "running"
    await asyncio.sleep(0.4)  # 2nd job supersedes the inbox subscription
    q = _watch(seeded_kernel, xid)
    await seeded_kernel.send(xid, {"type": "send", "text": "once", "client_id": "c"})
    says = await _collect_says(q, "once")
    assert len(says) == 1, f"expected exactly one echo after restart, got {len(says)}"
    await seeded_kernel.send(xid, {"type": "stop"})


# ─── the kernel TASK TABLE: cancel REAPS the OS process ─────────


async def test_a_job_holds_a_row_in_the_kernel_task_table(seeded_kernel):
    """A job OUTLIVES the `start` dispatch, so it registers its own task row — without it
    the kernel loses sight of the subprocess the moment `start` replies, and an
    unreachable subprocess is an orphan waiting to happen."""
    xid = await _wired(seeded_kernel)
    r = await seeded_kernel.send(
        xid, {"type": "start", "code": "import time\ntime.sleep(30)"}
    )
    snap = (await seeded_kernel.send(xid, {"type": "status", "job_id": r["job_id"]}))[
        "jobs"
    ][0]
    task_id = snap["task_id"]
    assert task_id and task_id.startswith(xid + ":")
    rows = (await seeded_kernel.send("kernel", {"type": "ps"}))["tasks"]
    row = next(t for t in rows if t["id"] == task_id)
    assert row["agent"] == xid and row["verb"] == "start"
    await seeded_kernel.send(xid, {"type": "stop"})


async def test_cancel_really_reaps_the_os_subprocess(seeded_kernel):
    """Cancelling an asyncio.Task kills NO OS process — the bundle's `on_cancel` hook
    does, and the kernel awaits it BEFORE aborting the task. Assert the PROCESS is gone
    (`os.kill` raises), not merely that the task object was cancelled."""
    xid = await _wired(seeded_kernel)
    r = await seeded_kernel.send(
        xid, {"type": "start", "code": "import time\nwhile True: time.sleep(1)"}
    )
    jid = r["job_id"]
    process = xb._jobs[jid]["proc"]
    os_pid = process.pid
    assert process.returncode is None  # the child is really running
    task_id = xb._jobs[jid]["task_id"]

    got = await seeded_kernel.send("kernel", {"type": "cancel", "task_id": task_id})
    assert got == {"cancelled": True, "killed": [task_id]}

    assert await _dead(process), f"pid {os_pid} survived cancel — ORPHANED"
    assert process.returncode != 0  # died by signal
    with pytest.raises(ProcessLookupError):
        os.kill(os_pid, 0)

    rows = (await seeded_kernel.send("kernel", {"type": "ps"}))["tasks"]
    assert task_id not in {t["id"] for t in rows}
    snap = (await seeded_kernel.send(xid, {"type": "status", "job_id": jid}))["jobs"][0]
    assert snap["status"] == "stopped", "no row may be left stuck `running`"


async def test_deleting_the_agent_reaps_its_running_jobs(seeded_kernel):
    """`on_delete` is a cascade for REAL resources: dropping the record while a job runs
    would leave a process nothing can address."""
    xid = await _wired(seeded_kernel)
    r = await seeded_kernel.send(
        xid, {"type": "start", "code": "import time\nwhile True: time.sleep(1)"}
    )
    process = xb._jobs[r["job_id"]]["proc"]
    d = await seeded_kernel.send("kernel_state", {"type": "delete_agent", "id": xid})
    assert d.get("deleted") is True
    assert await _dead(process), "delete_agent orphaned the subprocess"


async def test_a_durable_tool_self_reaps_when_its_control_socket_eofs(seeded_kernel):
    """A tool parked on its inbox must SELF-REAP when the control fd EOFs — i.e. when the
    kernel is gone — or it lingers as an orphan forever. Tear down ONLY the control relay
    (exactly what a kernel-process exit does to the parent socket) and call NO kill: the
    child must exit cleanly, by itself."""
    xid = await _wired(seeded_kernel, id="xb_reap", shim="python", code=_ECHO)
    await asyncio.sleep(0.4)  # boot → durable job parked on Event().wait()
    job = next(v for v in xb._jobs.values() if v["owner"] == xid)
    process = job["proc"]
    assert process.returncode is None, "durable tool should be parked + alive"

    job["ctrl_task"].cancel()
    try:
        await job["ctrl_task"]
    except asyncio.CancelledError:
        pass

    # Without the self-reap this hangs (orphan); with it the child exits cleanly.
    await asyncio.wait_for(process.wait(), timeout=5.0)
    assert process.returncode == 0, "child must self-reap on EOF, not orphan"
