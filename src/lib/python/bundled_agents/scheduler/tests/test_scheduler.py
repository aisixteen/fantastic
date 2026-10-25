"""scheduler bundle — recurring tasks (persists THROUGH the loader)."""

from __future__ import annotations

import yaml
import asyncio
import time
import types
from kernel import register_unlisted


async def _make_scheduler(kernel):
    """A scheduler — persists THROUGH the loader; nothing to wire. (Persistence tests
    also depend on the `store_agent` fixture so the loader has a store.)"""
    rec = (
        await kernel.send(
            [
                {
                    "target": "kernel_state",
                    "payload": {
                        "type": "create_agent",
                        "parent_id": "kernel_state",
                        "handler_module": "scheduler.tools",
                        # Deny-all: firing at `cli` is lateral egress → an explicit grant.
                        # (These tests all schedule against cli.)
                        "can_call": {"cli": "*"},
                    },
                }
            ]
        )
    )[0]["result"]
    sid = rec["id"]
    # Two-sided deny-all: a fire is a `send` to cli — open cli's reciprocal ingress.
    cli = kernel.ctx.agents.get("cli")
    cb = dict(cli._meta.get("callable_by") or {}) if cli is not None else {}
    cb[sid] = "*"
    (
        await kernel.send(
            [
                {
                    "target": "kernel_state",
                    "payload": {"type": "update_agent", "id": "cli", "callable_by": cb},
                }
            ]
        )
    )[0]["result"]
    return sid


async def test_docs_list_the_verbs_and_reflect_does_not(seeded_kernel, store_agent):
    sid = await _make_scheduler(seeded_kernel)
    r = (await seeded_kernel.send([{"target": sid, "payload": {"type": "reflect"}}]))[
        0
    ]["result"]
    assert "verbs" not in r and "emits" not in r
    d = (
        await seeded_kernel.send(
            [
                {
                    "target": "kernel",
                    "payload": {"type": "docs", "of": sid, "tier": "detailed"},
                }
            ]
        )
    )[0]["result"]
    assert "schedule" in d["verbs"] and "schedule_fired" in d["emits"]


async def test_boot_succeeds_without_store(seeded_kernel):
    # boot no longer needs a store — it starts the tick loop; writes failfast lazily.
    sid = await _make_scheduler(seeded_kernel)
    r = (await seeded_kernel.send([{"target": sid, "payload": {"type": "boot"}}]))[0][
        "result"
    ]
    assert r.get("running") is True


async def test_schedule_failfasts_without_store(seeded_kernel):
    # No store wired at the loader ⇒ the write (a schedule) failfasts, no phantom.
    sid = await _make_scheduler(seeded_kernel)
    r = (
        await seeded_kernel.send(
            [
                {
                    "target": sid,
                    "payload": {
                        "type": "schedule",
                        "target": "cli",
                        "payload": {"type": "say", "text": "x"},
                        "interval_seconds": 1,
                    },
                }
            ]
        )
    )[0]["result"]
    assert "error" in r and "no store" in r["error"]
    # rolled back — nothing lingers
    assert (
        (
            (await seeded_kernel.send([{"target": sid, "payload": {"type": "list"}}]))[
                0
            ]["result"]
        )["schedules"]
        == []
    )


async def test_schedule_persists_through_loader(seeded_kernel, store_agent, tmp_path):
    sid = await _make_scheduler(seeded_kernel)
    r = (
        await seeded_kernel.send(
            [
                {
                    "target": sid,
                    "payload": {
                        "type": "schedule",
                        "target": "cli",
                        "payload": {"type": "say", "text": "tick"},
                        "interval_seconds": 60,
                    },
                }
            ]
        )
    )[0]["result"]
    assert "schedule_id" in r
    persisted = (tmp_path / ".fantastic" / "agents" / sid / "schedules.yaml").read_text(
        encoding="utf-8"
    )
    assert r["schedule_id"] in persisted


async def test_schedule_validation(seeded_kernel, store_agent):
    sid = await _make_scheduler(seeded_kernel)
    no_target = (
        await seeded_kernel.send(
            [
                {
                    "target": sid,
                    "payload": {
                        "type": "schedule",
                        "payload": {"type": "x"},
                        "interval_seconds": 5,
                    },
                }
            ]
        )
    )[0]["result"]
    assert "error" in no_target
    no_payload_type = (
        await seeded_kernel.send(
            [
                {
                    "target": sid,
                    "payload": {
                        "type": "schedule",
                        "target": "cli",
                        "payload": {},
                        "interval_seconds": 5,
                    },
                }
            ]
        )
    )[0]["result"]
    assert "error" in no_payload_type


async def test_unschedule(seeded_kernel, store_agent):
    sid = await _make_scheduler(seeded_kernel)
    r = (
        await seeded_kernel.send(
            [
                {
                    "target": sid,
                    "payload": {
                        "type": "schedule",
                        "target": "cli",
                        "payload": {"type": "say", "text": "x"},
                        "interval_seconds": 60,
                    },
                }
            ]
        )
    )[0]["result"]
    sch_id = r["schedule_id"]
    out = (
        await seeded_kernel.send(
            [{"target": sid, "payload": {"type": "unschedule", "schedule_id": sch_id}}]
        )
    )[0]["result"]
    assert out["removed"] is True


async def test_list(seeded_kernel, store_agent):
    sid = await _make_scheduler(seeded_kernel)
    (
        await seeded_kernel.send(
            [
                {
                    "target": sid,
                    "payload": {
                        "type": "schedule",
                        "target": "cli",
                        "payload": {"type": "say", "text": "x"},
                        "interval_seconds": 60,
                    },
                }
            ]
        )
    )[0]["result"]
    r = (await seeded_kernel.send([{"target": sid, "payload": {"type": "list"}}]))[0][
        "result"
    ]
    assert len(r["schedules"]) == 1


async def test_pause_resume_one(seeded_kernel, store_agent):
    sid = await _make_scheduler(seeded_kernel)
    r = (
        await seeded_kernel.send(
            [
                {
                    "target": sid,
                    "payload": {
                        "type": "schedule",
                        "target": "cli",
                        "payload": {"type": "say", "text": "x"},
                        "interval_seconds": 60,
                    },
                }
            ]
        )
    )[0]["result"]
    sch_id = r["schedule_id"]
    p = (
        await seeded_kernel.send(
            [{"target": sid, "payload": {"type": "pause", "schedule_id": sch_id}}]
        )
    )[0]["result"]
    assert p["paused"] is True
    r = (
        await seeded_kernel.send(
            [{"target": sid, "payload": {"type": "resume", "schedule_id": sch_id}}]
        )
    )[0]["result"]
    assert r["resumed"] is True


async def test_tick_now_fires(seeded_kernel, store_agent, tmp_path):
    sid = await _make_scheduler(seeded_kernel)
    r = (
        await seeded_kernel.send(
            [
                {
                    "target": sid,
                    "payload": {
                        "type": "schedule",
                        "target": "cli",
                        "payload": {"type": "say", "text": "x"},
                        "interval_seconds": 60,
                    },
                }
            ]
        )
    )[0]["result"]
    sch_id = r["schedule_id"]
    out = (
        await seeded_kernel.send(
            [{"target": sid, "payload": {"type": "tick_now", "schedule_id": sch_id}}]
        )
    )[0]["result"]
    assert out["fired"] is True
    assert "result" in out and "error" in out, out
    # schedule_events.yaml now exists
    hist_path = tmp_path / ".fantastic" / "agents" / sid / "schedule_events.yaml"
    assert hist_path.exists()
    assert "schedule_fired" in hist_path.read_text(encoding="utf-8")


async def test_tick_now_reports_a_refused_fire(seeded_kernel, store_agent):
    """`tick_now` answers the fire's OUTCOME: a schedule sending `{type:'tick'}` to
    an AI agent, which has no such verb, is refused, and `{fired:true}` alone would
    hide that from the model."""
    sid = await _make_scheduler(seeded_kernel)
    r = (
        await seeded_kernel.send(
            [
                {
                    "target": sid,
                    "payload": {
                        "type": "schedule",
                        "target": "ghost",
                        "payload": {"type": "tick"},
                        "interval_seconds": 60,
                    },
                }
            ]
        )
    )[0]["result"]
    out = (
        await seeded_kernel.send(
            [
                {
                    "target": sid,
                    "payload": {"type": "tick_now", "schedule_id": r["schedule_id"]},
                }
            ]
        )
    )[0]["result"]
    assert out["fired"] is True
    said = out["error"] or (out["result"] or {}).get("error")
    assert said and "ghost" in str(said), out


async def test_history(seeded_kernel, store_agent):
    sid = await _make_scheduler(seeded_kernel)
    r = (
        await seeded_kernel.send(
            [
                {
                    "target": sid,
                    "payload": {
                        "type": "schedule",
                        "target": "cli",
                        "payload": {"type": "say", "text": "x"},
                        "interval_seconds": 60,
                    },
                }
            ]
        )
    )[0]["result"]
    sch_id = r["schedule_id"]
    (
        await seeded_kernel.send(
            [{"target": sid, "payload": {"type": "tick_now", "schedule_id": sch_id}}]
        )
    )[0]["result"]
    h = (
        await seeded_kernel.send(
            [{"target": sid, "payload": {"type": "history", "limit": 10}}]
        )
    )[0]["result"]
    assert h["count"] >= 1


async def test_fire_emits_to_target_inbox(seeded_kernel, store_agent):
    sid = await _make_scheduler(seeded_kernel)
    r = (
        await seeded_kernel.send(
            [
                {
                    "target": sid,
                    "payload": {
                        "type": "schedule",
                        "target": "cli",
                        "payload": {"type": "say", "text": "x"},
                        "interval_seconds": 60,
                    },
                }
            ]
        )
    )[0]["result"]
    sch_id = r["schedule_id"]
    # THE READERS REGISTER FIRST. A queue exists iff someone called
    # `ensure_inbox`, so a reader that arrives after the tick sees nothing.
    q = seeded_kernel.ctx.ensure_inbox("cli")
    own = seeded_kernel.ctx.ensure_inbox(sid)
    (
        await seeded_kernel.send(
            [{"target": sid, "payload": {"type": "tick_now", "schedule_id": sch_id}}]
        )
    )[0]["result"]
    # ONE delivery per tick: the target gets the PAYLOAD and nothing else. Echoing
    # `schedule_fired` at the target too would make a live program's on_message see
    # every tick twice. Telemetry rides the scheduler's OWN inbox + history.
    msgs = []
    while not q.empty():
        msgs.append(q.get_nowait())
    types = [m["type"] for m in msgs]
    assert "say" in types
    assert "schedule_fired" not in types, "telemetry echoed at the target again"
    own_types = []
    while not own.empty():
        own_types.append(own.get_nowait()["type"])
    assert "schedule_fired" in own_types, "the scheduler's own inbox lost the event"


async def test_unknown_verb_errors(seeded_kernel, store_agent):
    sid = await _make_scheduler(seeded_kernel)
    r = (await seeded_kernel.send([{"target": sid, "payload": {"type": "garbage"}}]))[
        0
    ]["result"]
    assert "error" in r


# ─── #145: one concurrent batch per tick, persist-before-fire, per-schedule timeout ──


# A probe target: `hit` records when it ran (and what the scheduler had
# persisted at that moment), optionally blocking for `block` seconds.
_probe = types.ModuleType("sched_probe.tools")
_probe.hits = {}  # agent id -> {"at": float, "doc": list|None, "overlapped": bool}
_probe.capture_path = None
_probe.started = {}  # agent id -> asyncio.Event, set the moment its hit begins


async def _probe_handler(id, payload, kernel):
    verb = payload.get("type")
    if verb in ("boot", "shutdown"):
        return None
    if verb == "reflect":
        return {"id": id, "sentence": "scheduler probe", "verbs": {}}
    if verb == "hit":
        doc = None
        p = _probe.capture_path
        if p is not None and p.exists():
            doc = yaml.safe_load(p.read_text(encoding="utf-8"))
        _probe.hits[id] = {"at": time.time(), "doc": doc}
        _probe.started.setdefault(id, asyncio.Event()).set()
        if other := payload.get("wait_for"):
            # CONCURRENCY, observed rather than timed: this hit holds until the
            # other one has STARTED. Two hits of one batch overlap, so it arrives;
            # fired one after the other, it cannot, and the hit gives up.
            ev = _probe.started.setdefault(other, asyncio.Event())
            try:
                await asyncio.wait_for(ev.wait(), float(payload.get("block", 3.0)))
                _probe.hits[id]["overlapped"] = True
            except asyncio.TimeoutError:
                _probe.hits[id]["overlapped"] = False
        elif payload.get("block"):
            await asyncio.sleep(float(payload["block"]))
        return {"hit": id}
    return {"error": f"unknown verb {verb!r}"}


_probe.handler = _probe_handler
_probe.VERBS = {}
register_unlisted("sched_probe.tools", _probe)


async def _make_probed_scheduler(kernel, sched_id: str, probes: list[str]):
    """A scheduler (explicit id) + probe targets, both sides of the
    deny-all gate opened (scheduler can_call probes, probes callable_by it)."""
    _probe.hits.clear()
    _probe.capture_path = None
    for pid in probes:
        r = (
            await kernel.send(
                [
                    {
                        "target": "kernel_state",
                        "payload": {
                            "type": "create_agent",
                            "parent_id": "kernel_state",
                            "handler_module": "sched_probe.tools",
                            "id": pid,
                            "callable_by": {sched_id: "*"},
                        },
                    }
                ]
            )
        )[0]["result"]
        assert r.get("id") == pid, r
    r = (
        await kernel.send(
            [
                {
                    "target": "kernel_state",
                    "payload": {
                        "type": "create_agent",
                        "parent_id": "kernel_state",
                        "handler_module": "scheduler.tools",
                        "id": sched_id,
                        "can_call": {p: "*" for p in probes},
                    },
                }
            ]
        )
    )[0]["result"]
    assert r.get("id") == sched_id, r
    return sched_id


async def _schedule(kernel, sid, target, interval=1, **extra):
    return (
        await kernel.send(
            [
                {
                    "target": sid,
                    "payload": {
                        "type": "schedule",
                        "target": target,
                        "payload": {"type": "hit", **extra.pop("hit", {})},
                        "interval_seconds": interval,
                        **extra,
                    },
                }
            ]
        )
    )[0]["result"]


async def _wait_for(pred, seconds: float):
    deadline = time.time() + seconds
    while time.time() < deadline:
        if pred():
            return True
        await asyncio.sleep(0.05)
    return pred()


async def test_due_schedules_fire_as_one_concurrent_batch(seeded_kernel, store_agent):
    # Two due schedules; the FIRST one holds until the second has STARTED.
    # Sequential firing cannot get there (the second starts only after the
    # first returns); one batch starts both together. Observed, not timed — a
    # wall-clock gap failed under a loaded test run with the batch intact.
    sid = await _make_probed_scheduler(seeded_kernel, "sch_batch", ["p_slow", "p_fast"])
    _probe.started.clear()
    # BOTH DUE when the loop looks: the loop fires at each schedule's exact due
    # time (no tick to coalesce on), so the two are made due while the scheduler
    # is paused, and the resume wakes it to one batch.
    (await seeded_kernel.send([{"target": sid, "payload": {"type": "pause"}}]))[0]
    r1 = await _schedule(
        seeded_kernel, sid, "p_slow", hit={"wait_for": "p_fast", "block": 3.0}
    )
    r2 = await _schedule(seeded_kernel, sid, "p_fast")
    assert "schedule_id" in r1 and "schedule_id" in r2
    await asyncio.sleep(1.2)
    (await seeded_kernel.send([{"target": sid, "payload": {"type": "resume"}}]))[0]
    assert await _wait_for(lambda: {"p_slow", "p_fast"} <= set(_probe.hits), 3.0), (
        _probe.hits
    )
    assert await _wait_for(lambda: "overlapped" in _probe.hits["p_slow"], 5.0)
    assert _probe.hits["p_slow"]["overlapped"], (
        "not one batch: the hits never overlapped"
    )
    (await seeded_kernel.send([{"target": sid, "payload": {"type": "pause"}}]))[0]
    listed = (await seeded_kernel.send([{"target": sid, "payload": {"type": "list"}}]))[
        0
    ]["result"]["schedules"]
    assert all(s["run_count"] >= 1 for s in listed), listed


async def test_next_run_is_persisted_before_the_fire(
    seeded_kernel, store_agent, tmp_path
):
    # The probe reads schedules.yaml the moment it is hit: the store must
    # already hold the bumped run_count/next_run (persist-BEFORE-fire).
    sid = await _make_probed_scheduler(seeded_kernel, "sch_persist", ["p_cap"])
    _probe.capture_path = tmp_path / ".fantastic" / "agents" / sid / "schedules.yaml"
    r = await _schedule(seeded_kernel, sid, "p_cap", interval=1)
    sch_id = r["schedule_id"]
    (await seeded_kernel.send([{"target": sid, "payload": {"type": "boot"}}]))[0]
    assert await _wait_for(lambda: "p_cap" in _probe.hits, 3.0)
    (await seeded_kernel.send([{"target": sid, "payload": {"type": "pause"}}]))[0]
    hit = _probe.hits["p_cap"]
    assert hit["doc"] is not None, "store had no schedules.yaml at fire time"
    rec = next(s for s in hit["doc"] if s["id"] == sch_id)
    assert rec["run_count"] == 1, rec
    assert rec["next_run"] > hit["at"] - 0.5, (rec, hit["at"])


async def test_a_schedule_timeout_bounds_its_fire(seeded_kernel, store_agent):
    sid = await _make_probed_scheduler(seeded_kernel, "sch_timeout", ["p_hang"])
    r = await _schedule(
        seeded_kernel, sid, "p_hang", interval=60, fire_timeout=0.1, hit={"block": 2.0}
    )
    sch_id = r["schedule_id"]
    assert r["schedule"]["fire_timeout"] == 0.1
    t0 = time.time()
    fired = (
        await seeded_kernel.send(
            [{"target": sid, "payload": {"type": "tick_now", "schedule_id": sch_id}}]
        )
    )[0]["result"]
    assert fired.get("fired") is True, fired
    assert time.time() - t0 < 1.5, "fire was not bounded by the schedule timeout"
    h = (
        await seeded_kernel.send(
            [{"target": sid, "payload": {"type": "history", "schedule_id": sch_id}}]
        )
    )[0]["result"]
    assert h["count"] == 1, h
    assert "timed_out" in (h["history"][0]["error"] or ""), h["history"][0]


async def test_schedule_rejects_a_bad_timeout(seeded_kernel, store_agent):
    sid = await _make_probed_scheduler(seeded_kernel, "sch_badto", ["p_x"])
    for bad_value in (0, -1, "5", True):
        r = await _schedule(seeded_kernel, sid, "p_x", fire_timeout=bad_value)
        assert r.get("reason") == "bad_arg" and r.get("arg") == "fire_timeout", r
    listed = (await seeded_kernel.send([{"target": sid, "payload": {"type": "list"}}]))[
        0
    ]["result"]["schedules"]
    assert listed == []


async def test_non_int_args_are_bad_arg_refusals(seeded_kernel, store_agent):
    """A present-but-wrong number is a TYPO, not a new default: neither raised out
    of the handler by `int()` nor swallowed by `or 60` — either way the caller's
    mistake would become a silent behaviour change."""
    sid = await _make_probed_scheduler(seeded_kernel, "sch_ints", ["p_x"])
    for bad_value in ("60", 1.5, True, None, 0, -1, [60]):
        r = await _schedule(seeded_kernel, sid, "p_x", interval=bad_value)
        assert r.get("reason") == "bad_arg", (bad_value, r)
        assert r.get("arg") == "interval_seconds", (bad_value, r)
    # Nothing was scheduled by a refused call.
    listed = (await seeded_kernel.send([{"target": sid, "payload": {"type": "list"}}]))[
        0
    ]["result"]["schedules"]
    assert listed == []
    # ABSENT still takes the documented default (60s), and is NOT refused.
    ok = (
        await seeded_kernel.send(
            [
                {
                    "target": sid,
                    "payload": {
                        "type": "schedule",
                        "target": "p_x",
                        "payload": {"type": "hit"},
                    },
                }
            ]
        )
    )[0]["result"]
    assert ok["schedule"]["interval_seconds"] == 60, ok

    for bad_value in ("100", 2.0, True, None, 0):
        h = (
            await seeded_kernel.send(
                [{"target": sid, "payload": {"type": "history", "limit": bad_value}}]
            )
        )[0]["result"]
        assert h.get("reason") == "bad_arg" and h.get("arg") == "limit", (bad_value, h)
    # ABSENT `limit` is the default page, not a refusal.
    h = (await seeded_kernel.send([{"target": sid, "payload": {"type": "history"}}]))[
        0
    ]["result"]
    assert "history" in h and "reason" not in h, h


async def test_schedule_refuses_a_non_object_payload(seeded_kernel, store_agent):
    """`payload: [1]` is refused, never raised `.get` out of the handler."""
    sid = await _make_probed_scheduler(seeded_kernel, "sch_payload", ["p_x"])
    for bad_value in ([1], "hit", 7):
        r = (
            await seeded_kernel.send(
                [
                    {
                        "target": sid,
                        "payload": {
                            "type": "schedule",
                            "target": "p_x",
                            "payload": bad_value,
                        },
                    }
                ]
            )
        )[0]["result"]
        assert r.get("reason") == "bad_arg" and r.get("arg") == "payload", (
            bad_value,
            r,
        )


async def test_an_idle_scheduler_never_wakes_and_a_change_wakes_it(
    seeded_kernel, store_agent, monkeypatch
):
    """No idle wakeups: the loop sleeps until the earliest due
    fire, and with nothing to wait for until a change — a schedule added, or the
    scheduler's own record updated (`paused`, even by update_agent)."""
    from scheduler import tools

    looks = 0
    real_load = tools._load

    async def counting_load(sid, kernel):
        nonlocal looks
        looks += 1
        return await real_load(sid, kernel)

    monkeypatch.setattr(tools, "_load", counting_load)
    sid = await _make_probed_scheduler(seeded_kernel, "sch_idle", ["p_idle"])
    await asyncio.sleep(0.5)
    idle = looks
    await asyncio.sleep(1.0)
    assert looks == idle, f"an idle scheduler woke {looks - idle} times"
    # a schedule wakes it; it fires at its due time, not on a tick
    await _schedule(seeded_kernel, sid, "p_idle", interval=1)
    assert await _wait_for(lambda: "p_idle" in _probe.hits, 3.0), _probe.hits
    # paused by update_agent: the loop waits on nothing; resumed the same way, it fires
    up = {"type": "update_agent", "id": sid}
    await seeded_kernel.send(
        [{"target": "kernel_state", "payload": {**up, "paused": True}}]
    )
    await asyncio.sleep(0.1)
    _probe.hits.clear()
    before = looks
    await asyncio.sleep(1.5)
    assert "p_idle" not in _probe.hits and looks - before <= 1, looks - before
    await seeded_kernel.send(
        [{"target": "kernel_state", "payload": {**up, "paused": False}}]
    )
    assert await _wait_for(lambda: "p_idle" in _probe.hits, 3.0), _probe.hits


async def test_missing_and_unknown_schedule_ids(seeded_kernel, store_agent):
    sid = await _make_scheduler(seeded_kernel)

    async def call(payload):
        return (await seeded_kernel.send([{"target": sid, "payload": payload}]))[0][
            "result"
        ]

    assert await call({"type": "unschedule"}) == {
        "error": "unschedule: schedule_id required"
    }
    assert await call({"type": "tick_now"}) == {
        "error": "tick_now: schedule_id required"
    }
    assert await call({"type": "tick_now", "schedule_id": "nope"}) == {
        "error": "schedule 'nope' not found"
    }
    assert await call({"type": "unschedule", "schedule_id": "nope"}) == {
        "removed": False,
        "schedule_id": "nope",
    }
    assert await call({"type": "pause", "schedule_id": "nope"}) == {
        "paused": False,
        "schedule_id": "nope",
    }
    assert await call({"type": "resume", "schedule_id": "nope"}) == {
        "resumed": False,
        "schedule_id": "nope",
    }


async def test_pause_and_resume_the_whole_scheduler(seeded_kernel, store_agent):
    sid = await _make_scheduler(seeded_kernel)

    async def call(payload):
        return (await seeded_kernel.send([{"target": sid, "payload": payload}]))[0][
            "result"
        ]

    assert await call({"type": "pause"}) == {"paused": True, "scheduler_id": sid}
    assert seeded_kernel.ctx.get(sid)["paused"] is True
    assert await call({"type": "resume"}) == {"resumed": True, "scheduler_id": sid}
    assert seeded_kernel.ctx.get(sid)["paused"] is False


async def _send(kernel, target, payload):
    return (await kernel.send([{"target": target, "payload": payload}]))[0]["result"]


def _seed_schedule(sid, sch_id, target, **over):
    from scheduler import tools

    now = time.time()
    sch = {
        "id": sch_id,
        "target": target,
        "payload": {"type": "hit"},
        "interval_seconds": 60,
        "created_at": now - 120,
        "next_run": now + 60,
        "run_count": 0,
        "paused": False,
        **over,
    }
    tools._cache[sid] = [sch]
    return sch


async def test_a_failed_tick_persist_does_not_refire_next_tick(seeded_kernel):
    # No store_agent: every persist fails. The bump must stay in the cache, or the
    # still-due schedule would fire again in a hot loop.
    from scheduler import tools

    sid = await _make_probed_scheduler(seeded_kernel, "sch_nopersist", ["p_np"])
    try:
        _seed_schedule(sid, "sch_fixed", "p_np", next_run=time.time() - 1)
        await _send(seeded_kernel, sid, {"type": "boot"})
        assert await _wait_for(lambda: "p_np" in _probe.hits, 3.0), _probe.hits
        await asyncio.sleep(0.6)
        await _send(seeded_kernel, sid, {"type": "pause"})
        schedules = (await _send(seeded_kernel, sid, {"type": "list"}))["schedules"]
        assert schedules[0]["run_count"] == 1, "the failed persist re-fired the tick"
        assert schedules[0]["next_run"] > time.time()
    finally:
        tools._cache.pop(sid, None)


async def test_a_failed_persist_rolls_back_tick_now_but_not_pause(seeded_kernel):
    from scheduler import tools

    sid = await _make_probed_scheduler(seeded_kernel, "sch_rb", ["p_rb"])
    try:
        _seed_schedule(sid, "sch_rb", "p_rb")
        r = await _send(seeded_kernel, sid, {"type": "tick_now", "schedule_id": "sch_rb"})
        assert isinstance(r.get("error"), str), r
        assert tools._cache[sid][0]["run_count"] == 0, "the tick_now bump was kept"
        r = await _send(seeded_kernel, sid, {"type": "pause", "schedule_id": "sch_rb"})
        assert isinstance(r.get("error"), str), r
        assert tools._cache[sid][0]["paused"] is True, "the pause was rolled back"
    finally:
        tools._cache.pop(sid, None)


async def test_two_schedulers_share_nothing_and_delete_frees_only_its_own(
    seeded_kernel, store_agent
):
    from scheduler import tools

    a = await _make_probed_scheduler(seeded_kernel, "sch_two_a", ["p_two_a"])
    b = await _make_probed_scheduler(seeded_kernel, "sch_two_b", ["p_two_b"])
    try:
        for s in (a, b):
            assert (await _send(seeded_kernel, s, {"type": "boot"}))["running"] is True
        r = await _schedule(seeded_kernel, a, "p_two_a", interval=3600)
        assert "schedule_id" in r, r
        assert len((await _send(seeded_kernel, a, {"type": "list"}))["schedules"]) == 1
        assert (await _send(seeded_kernel, b, {"type": "list"}))["schedules"] == []
        for s in (a, b):
            assert (await _send(seeded_kernel, s, {"type": "reflect"}))["running"]
            assert s in tools._tick_tasks and not tools._tick_tasks[s].done()
        d = await _send(seeded_kernel, "kernel_state", {"type": "delete_agent", "id": a})
        assert "error" not in d, d
        await asyncio.sleep(0.1)
        assert a not in tools._tick_tasks
        assert a not in tools._cache
        assert a not in tools._wake
        assert a not in tools._unsub
        assert b in tools._tick_tasks and not tools._tick_tasks[b].done()
        assert b in tools._wake and b in tools._unsub
        assert (await _send(seeded_kernel, b, {"type": "reflect"}))["running"] is True
        assert (await _send(seeded_kernel, b, {"type": "list"}))["schedules"] == []
    finally:
        for s in (a, b):
            tools._cache.pop(s, None)


async def test_a_cancelled_fire_error_reads_none():
    from scheduler import tools

    class FakeKernel:
        def __init__(self):
            self.outcome = None
            self.ctx = types.SimpleNamespace(root=None)

        async def send(self, items):
            return [self.outcome for _ in items]

        async def emit(self, sid, event):
            return None

    fake = FakeKernel()
    due = [{"id": "sch_x", "target": "t", "payload": {"type": "hit"}}]
    cases = [
        ({"status": "cancelled", "result": None}, (None, "cancelled: None")),
        ({"status": "cancelled", "result": {}}, (None, "cancelled: None")),
        (
            {"status": "timed_out", "result": {"error": "gone"}},
            (None, "timed_out: gone"),
        ),
        ({"status": "failed", "result": [1, "x"]}, (None, "failed: [1, 'x']")),
        (
            {"status": "completed", "result": {"error": "domain said no"}},
            ({"error": "domain said no"}, None),
        ),
    ]
    for outcome, want in cases:
        fake.outcome = outcome
        (ev,) = await tools._fire_many("sch_cx", due, fake)
        assert (ev["result"], ev["error"]) == want, (outcome, ev)
