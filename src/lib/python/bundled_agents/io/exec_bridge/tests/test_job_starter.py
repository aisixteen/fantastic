"""A job is DRIVEN by the caller that started it — the operator, always.

Audit AV-8 (2026-10-03): every job verb checked only that the job belonged to
THIS exec_bridge agent, so any caller granted `write` on a shared leg typed into
another caller's shell (a shell job carries the agent's FANTASTIC_CTRL_FD
identity) and killed its jobs. Measured on the old code: `b` wrote 7 bytes into
`a`'s PTY. Now `start` records the job's `starter` (the calling agent, a foreign
caller's `<guid>|<agent>`, or null for the operator) and the verbs that DRIVE a
job — `write`, `resize`, `paste_image`, `stop`, `interrupt`, `clear` — refuse a
named job another caller started; their bulk forms act on the caller's own jobs.

The READ verbs — `status`, `output`, `ack` — stay gated by grants alone: the
documented read-only viewer (granted `status`/`output`/`ack`, never `start`) is
by construction NOT the starter, so scoping them to the starter would end it.
"""

from __future__ import annotations

import sys

import pytest

from exec_bridge import tools as ebt
from kernel import sender_context

PY = sys.executable
# Prints a line, then waits for one: a live PTY job until it is written to.
_CHILD = "import sys\nprint('READY', flush=True)\nprint('GOT', sys.stdin.readline().strip(), flush=True)\n"


async def _call(k, payload, as_=None, target="term"):
    item = [{"target": target, "payload": payload}]
    if as_ is None:
        return (await k.send(item))[0]["result"]
    with sender_context(as_):
        return (await k.send(item))[0]["result"]


@pytest.fixture
async def term(seeded_kernel):
    k = seeded_kernel
    made = (
        await k.send(
            [
                {
                    "target": k.id,
                    "payload": {
                        "type": "create_agent",
                        "parent_id": k.id,
                        "id": "term",
                        "handler_module": "exec_bridge.tools",
                        "ingress_rule": "allow_all_ingress",
                        "cwd": ".",
                        "allowed": [rf"{PY} -c .*"],
                        "callable_by": {"a": "*", "b": "*"},
                    },
                }
            ]
        )
    )[0]["result"]
    assert made.get("id") == "term", made
    for who in ("a", "b"):
        k.create(None, id=who, can_call={"term": "*"})
    yield k
    for j in list(ebt._jobs.values()):
        if j.get("owner") == "term":
            ebt._teardown(j)


async def _start_as(k, who):
    r = await _call(
        k,
        {
            "type": "start",
            "command": PY,
            "args": ["-c", _CHILD],
            "pty": {"cols": 80, "rows": 24},
        },
        as_=who,
    )
    assert r.get("pty") is True, r
    return r["job_id"]


def _not_yours(verb: str, jid: int) -> dict:
    return {
        "error": f"exec_bridge: job {jid} is not yours — only the caller that "
        f"started it (or the operator) may {verb} it",
        "reason": "unauthorized",
        "job_id": jid,
    }


async def test_another_caller_cannot_drive_a_job(term):
    k = term
    jid = await _start_as(k, "a")
    for verb, extra in (
        ("write", {"data": "hijack\n"}),
        ("resize", {"cols": 100, "rows": 30}),
        ("paste_image", {"data": b"\x89PNG", "mime": "image/png"}),
        ("stop", {}),
        ("interrupt", {}),
        ("clear", {}),
    ):
        out = await _call(k, {"type": verb, "job_id": jid, **extra}, as_="b")
        assert out == _not_yours(verb, jid), (verb, out)
    assert ebt._alive(ebt._jobs[jid]), "b's verbs left a's job running"
    # the bulk forms act on b's own jobs — none
    assert await _call(k, {"type": "stop"}, as_="b") == {"killed": 0}
    assert await _call(k, {"type": "interrupt"}, as_="b") == {"interrupted": 0}
    assert ebt._alive(ebt._jobs[jid])
    assert ebt._jobs[jid]["starter"] == "a"


async def test_a_read_only_viewer_reads_a_job_it_did_not_start(term):
    """The viewer pattern the readme documents: granted only `status`/`output`/
    `ack`, it re-attaches to a terminal someone else started."""
    k = term
    jid = await _start_as(k, "a")
    listed = await _call(k, {"type": "status"}, as_="b")
    assert [j["job_id"] for j in listed["jobs"]] == [jid], listed
    one = await _call(k, {"type": "status", "job_id": jid}, as_="b")
    assert one["jobs"][0]["job_id"] == jid, one
    ring = await _call(k, {"type": "output", "job_id": jid}, as_="b")
    assert ring["job_id"] == jid and isinstance(ring["output"], str), ring
    acked = await _call(k, {"type": "ack", "job_id": jid, "chars": 1}, as_="b")
    assert set(acked) == {"unacked", "paused"}, acked


async def test_the_starter_and_the_operator_drive_it(term):
    k = term
    jid = await _start_as(k, "a")
    assert await _call(
        k, {"type": "resize", "job_id": jid, "cols": 90, "rows": 30}, as_="a"
    ) == {
        "resized": True,
        "cols": 90,
        "rows": 30,
    }
    mine = await _call(k, {"type": "status"}, as_="a")
    assert [j["job_id"] for j in mine["jobs"]] == [jid], mine
    everyone = await _call(k, {"type": "status"})
    assert [j["job_id"] for j in everyone["jobs"]] == [jid], everyone
    assert await _call(k, {"type": "write", "job_id": jid, "data": "op\n"}) == {
        "written": 3
    }


async def test_an_operator_started_job_is_the_operators(term):
    k = term
    jid = await _start_as(k, None)
    assert ebt._jobs[jid]["starter"] is None
    out = await _call(k, {"type": "write", "job_id": jid, "data": "x\n"}, as_="a")
    assert out == _not_yours("write", jid), out
