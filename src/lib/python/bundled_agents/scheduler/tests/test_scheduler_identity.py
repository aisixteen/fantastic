"""A schedule fires AS ITS CREATOR — the scheduler is no deputy.

Audit AV-6 (2026-10-03): every fire went out under the SCHEDULER's identity, so
anyone allowed to call `schedule`/`tick_now` on it borrowed its `can_call`, and
`history` handed them the replies. Measured: an agent `b` that could not read the
vault directly scheduled a vault `read`, ticked it, and read TOP-SECRET out of
`history`. Now the schedule records WHO scheduled it (`owner`) and fires under
that identity, so a schedule can do only what its creator could; an operator-made
schedule fires as the operator. A caller lists, ticks, pauses, resumes,
unschedules and reads the history of ITS OWN schedules only (the operator: all).
"""

from __future__ import annotations

import pytest

from kernel import sender_context
from scheduler import tools as sched_tools

SECRET = "TOP-SECRET-PAYROLL"


@pytest.fixture(autouse=True)
def _fresh_scheduler_state():
    """ONE KERNEL PER PROCESS: the scheduler keeps its schedules in a module dict
    keyed by agent id, so every test here starts and ends with `sched` cleared."""
    sched_tools._cache.pop("sched", None)
    yield
    sched_tools._cache.pop("sched", None)
    t = sched_tools._tick_tasks.pop("sched", None)
    if t is not None and not t.done():
        t.cancel()


async def _op(k, target, payload, as_=None) -> dict:
    item = [{"target": target, "payload": payload}]
    if as_ is None:
        return (await k.send(item))[0]
    with sender_context(as_):
        return (await k.send(item))[0]


async def _create(k, **meta) -> dict:
    out = await _op(k, k.id, {"type": "create_agent", "parent_id": k.id, **meta})
    assert out["result"].get("id") == meta["id"], out
    return out["result"]


SCHED_VERBS = ["schedule", "tick_now", "history", "list", "unschedule", "pause"]


async def _wire(k):
    """`vault` (a file holding SECRET) answers the SCHEDULER, never `b`; `pad`
    answers `b` and not the scheduler; `b` may call the scheduler's verbs."""
    await _create(
        k,
        id="vault",
        handler_module="file_bridge.tools",
        allow=["secret.txt"],
        ingress_rule="allow_all_ingress",
        callable_by={"sched": "*"},
    )
    w = await _op(k, "vault", {"type": "write", "path": "secret.txt", "bytes": SECRET})
    assert w["status"] == "completed", w
    await _create(
        k,
        id="pad",
        handler_module="file_bridge.tools",
        allow=["note.txt"],
        ingress_rule="allow_all_ingress",
        callable_by={"b": "*"},
    )
    await _create(
        k,
        id="sched",
        handler_module="scheduler.tools",
        can_call={"vault": "*"},
        callable_by={"b": SCHED_VERBS},
    )
    k.create(None, id="b", can_call={"sched": SCHED_VERBS, "pad": "*"})


async def test_a_schedule_cannot_reach_what_its_creator_cannot(
    seeded_kernel, store_agent
):
    k = seeded_kernel
    await _wire(k)
    direct = await _op(k, "vault", {"type": "read", "path": "secret.txt"}, as_="b")
    assert direct["result"].get("reason") == "unauthorized", direct
    made = await _op(
        k,
        "sched",
        {
            "type": "schedule",
            "target": "vault",
            "payload": {"type": "read", "path": "secret.txt"},
            "interval_seconds": 3600,
        },
        as_="b",
    )
    sch = made["result"]["schedule"]
    assert sch["owner"] == "b", sch
    fired = await _op(
        k, "sched", {"type": "tick_now", "schedule_id": sch["id"]}, as_="b"
    )
    assert fired["result"] == {"fired": True, "schedule_id": sch["id"]}, fired
    hist = await _op(k, "sched", {"type": "history"}, as_="b")
    assert SECRET not in repr(hist), hist
    (event,) = hist["result"]["history"]
    assert event["owner"] == "b" and event["result"] is None, event
    assert "b may not call 'read' on vault" in event["error"], event


async def test_a_schedule_reaches_what_its_creator_can_and_its_deputy_cannot(
    seeded_kernel, store_agent
):
    k = seeded_kernel
    await _wire(k)
    made = await _op(
        k,
        "sched",
        {
            "type": "schedule",
            "target": "pad",
            "payload": {"type": "write", "path": "note.txt", "bytes": "from-b"},
            "interval_seconds": 3600,
        },
        as_="b",
    )
    sid = made["result"]["schedule_id"]
    await _op(k, "sched", {"type": "tick_now", "schedule_id": sid}, as_="b")
    read = await _op(k, "pad", {"type": "read", "path": "note.txt"})
    assert [ln["text"] for ln in read["result"]["lines"]] == ["from-b"], read


async def test_an_operator_schedule_fires_as_the_operator(seeded_kernel, store_agent):
    """No grant on either end: the operator's schedule is the operator's call."""
    k = seeded_kernel
    await _wire(k)
    made = await _op(
        k,
        "sched",
        {
            "type": "schedule",
            "target": "pad",
            "payload": {"type": "write", "path": "note.txt", "bytes": "from-op"},
            "interval_seconds": 3600,
        },
    )
    sch = made["result"]["schedule"]
    assert sch["owner"] is None, sch
    await _op(k, "sched", {"type": "tick_now", "schedule_id": sch["id"]})
    read = await _op(k, "pad", {"type": "read", "path": "note.txt"})
    assert [ln["text"] for ln in read["result"]["lines"]] == ["from-op"], read


async def test_a_caller_sees_and_drives_only_its_own_schedules(
    seeded_kernel, store_agent
):
    k = seeded_kernel
    await _wire(k)
    mine = await _op(
        k,
        "sched",
        {
            "type": "schedule",
            "target": "pad",
            "payload": {"type": "list"},
            "interval_seconds": 3600,
        },
        as_="b",
    )
    theirs = await _op(
        k,
        "sched",
        {
            "type": "schedule",
            "target": "vault",
            "payload": {"type": "read", "path": "secret.txt"},
            "interval_seconds": 3600,
        },
    )
    mine_id = mine["result"]["schedule_id"]
    op_id = theirs["result"]["schedule_id"]
    await _op(k, "sched", {"type": "tick_now", "schedule_id": op_id})
    listed = await _op(k, "sched", {"type": "list"}, as_="b")
    assert [s["id"] for s in listed["result"]["schedules"]] == [mine_id], listed
    everything = await _op(k, "sched", {"type": "list"})
    assert {s["id"] for s in everything["result"]["schedules"]} == {mine_id, op_id}
    hist = await _op(k, "sched", {"type": "history"}, as_="b")
    assert hist["result"]["count"] == 0 and SECRET not in repr(hist), hist
    for verb, gone in (
        ("tick_now", {"error": f"schedule {op_id!r} not found"}),
        ("pause", {"paused": False, "schedule_id": op_id}),
        ("unschedule", {"removed": False, "schedule_id": op_id}),
    ):
        out = await _op(k, "sched", {"type": verb, "schedule_id": op_id}, as_="b")
        assert out["result"] == gone, (verb, out)
    after = await _op(k, "sched", {"type": "list"})
    assert {s["id"] for s in after["result"]["schedules"]} == {mine_id, op_id}
    assert not any(s["paused"] for s in after["result"]["schedules"]), after
    op_hist = await _op(k, "sched", {"type": "history"})
    assert op_hist["result"]["count"] == 1, op_hist


async def test_a_foreign_callers_schedule_fires_as_that_caller_via_its_leg(
    seeded_kernel, store_agent
):
    """A call relayed by a bridge leg is the FOREIGN caller's (`<guid>|<agent>`):
    its schedule records the caller and the leg (`via`), and fires as that same
    pair — the target's `callable_by` judges the foreign id, as on the wire."""
    from kernel import remote_caller_context

    k = seeded_kernel
    await _wire(k)
    k.create(None, id="leg", can_call={"sched": SCHED_VERBS, "pad": "*"})
    for agent, verbs in (("sched", SCHED_VERBS), ("pad", "*")):
        cb = dict(k.ctx.agents[agent]._meta.get("callable_by") or {})
        cb["peer-g|x"] = verbs
        k.update(agent, callable_by=cb)
    with sender_context("leg"), remote_caller_context("leg", "peer-g|x"):
        made = (
            await k.send(
                [
                    {
                        "target": "sched",
                        "payload": {
                            "type": "schedule",
                            "target": "pad",
                            "payload": {
                                "type": "write",
                                "path": "note.txt",
                                "bytes": "from-peer",
                            },
                            "interval_seconds": 3600,
                        },
                    }
                ]
            )
        )[0]["result"]
    sch = made["schedule"]
    assert (sch["owner"], sch["via"]) == ("peer-g|x", "leg"), sch
    await _op(k, "sched", {"type": "tick_now", "schedule_id": sch["id"]})
    read = await _op(k, "pad", {"type": "read", "path": "note.txt"})
    assert [ln["text"] for ln in read["result"]["lines"]] == ["from-peer"], read


# ─── an owner key that names nobody (audit ARCH2-4) ─────────────────

_FIRES_ONLY_AS = (
    "; a schedule fires only as its recorded owner "
    "(null = the operator, a string = an agent)"
)
OWNER_ABSENT = (
    "scheduler: not fired — the schedule's owner key is absent" + _FIRES_ONLY_AS
)
OWNER_NOT_A_NAME = (
    "scheduler: not fired — the schedule's owner is not null or a string"
    + _FIRES_ONLY_AS
)


def _stored(sid: str, next_run: float, **owner) -> dict:
    """A schedule as the sidecar holds it, targeting `pad` (which answers `b`
    only — never the scheduler); `owner` is passed only when the test wants it."""
    return {
        "id": sid,
        "target": "pad",
        "payload": {"type": "write", "path": "note.txt", "bytes": "unowned"},
        "interval_seconds": 3600,
        "created_at": 0,
        "next_run": next_run,
        "run_count": 0,
        "paused": False,
        **owner,
    }


async def test_a_schedule_whose_owner_names_nobody_is_refused_not_fired(
    seeded_kernel, store_agent
):
    """An ABSENT `owner` key read as null fired with the operator's ungated
    authority (measured: an ownerless sidecar entry wrote `pad`, which refuses the
    scheduler). The key must be present — null or a string; anything else is
    refused, nothing is sent, and the history says why."""
    k = seeded_kernel
    await _wire(k)
    sched_tools._cache["sched"] = [
        _stored("s_absent", 1e12),
        _stored("s_number", 1e12, owner=5),
    ]
    for sid, why in (("s_absent", OWNER_ABSENT), ("s_number", OWNER_NOT_A_NAME)):
        out = await _op(k, "sched", {"type": "tick_now", "schedule_id": sid})
        assert out["result"] == {"error": why}, out
    read = await _op(k, "pad", {"type": "read", "path": "note.txt"})
    assert "unowned" not in repr(read), read
    hist = await _op(k, "sched", {"type": "history"})
    got = [
        (e["schedule_id"], e["owner"], e["result"], e["error"])
        for e in hist["result"]["history"]
    ]
    assert got == [
        ("s_absent", None, None, OWNER_ABSENT),
        ("s_number", 5, None, OWNER_NOT_A_NAME),
    ], hist


async def test_the_tick_loop_refuses_an_ownerless_schedule_and_fires_the_rest(
    seeded_kernel, store_agent
):
    """The tick path: the ownerless schedule is refused and recorded, the
    operator's (`owner: null`, PRESENT) fires as before — in the same sweep."""
    import asyncio

    k = seeded_kernel
    await _wire(k)
    sched_tools._cache["sched"] = [
        _stored("s_absent", 0),
        {
            **_stored("s_op", 0, owner=None),
            "target": "vault",
            "payload": {"type": "list"},
        },
    ]
    k.update("sched", tick_sec=0.05)
    for _ in range(100):
        await asyncio.sleep(0.05)
        hist = await _op(k, "sched", {"type": "history"})
        if hist["result"]["count"] >= 2:
            break
    by_id = {e["schedule_id"]: e for e in hist["result"]["history"]}
    assert by_id["s_absent"]["error"] == OWNER_ABSENT, hist
    assert by_id["s_op"]["error"] is None, hist
    read = await _op(k, "pad", {"type": "read", "path": "note.txt"})
    assert "unowned" not in repr(read), read


async def test_schedule_always_writes_the_owner_key(seeded_kernel, store_agent):
    """The operator's schedule stores `owner: null` — PRESENT, so it fires."""
    k = seeded_kernel
    await _wire(k)
    made = await _op(
        k,
        "sched",
        {"type": "schedule", "target": "vault", "payload": {"type": "list"}},
    )
    assert "owner" in made["result"]["schedule"], made
    assert made["result"]["schedule"]["owner"] is None, made
    stored = sched_tools._cache["sched"]
    assert all("owner" in s for s in stored), stored
