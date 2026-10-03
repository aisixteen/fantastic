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


async def test_reflect_lists_the_verbs(seeded_kernel, store_agent):
    sid = await _make_scheduler(seeded_kernel)
    r = (await seeded_kernel.send([{"target": sid, "payload": {"type": "reflect"}}]))[
        0
    ]["result"]
    assert "schedule" in r["verbs"]


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
    persisted = (
        tmp_path / ".fantastic" / "agents" / sid / "schedules.yaml"
    ).read_text()
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
    # schedule_events.yaml now exists
    hist_path = tmp_path / ".fantastic" / "agents" / sid / "schedule_events.yaml"
    assert hist_path.exists()
    assert "schedule_fired" in hist_path.read_text()


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
    # ONE delivery per tick: the target gets the PAYLOAD and nothing else. The old
    # contract also echoed `schedule_fired` at the target, so a live program's
    # on_message saw every tick twice (measured 2026-08-07 — opus had to design a
    # filter around it). Telemetry rides the scheduler's OWN inbox + history.
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
            doc = yaml.safe_load(p.read_text())
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
    """A scheduler (explicit id, tick_sec 0.1) + probe targets, both sides of the
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
                        "tick_sec": 0.1,
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
    r1 = await _schedule(
        seeded_kernel, sid, "p_slow", hit={"wait_for": "p_fast", "block": 3.0}
    )
    r2 = await _schedule(seeded_kernel, sid, "p_fast")
    assert "schedule_id" in r1 and "schedule_id" in r2
    (await seeded_kernel.send([{"target": sid, "payload": {"type": "boot"}}]))[0]
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
        seeded_kernel, sid, "p_hang", interval=60, timeout=0.1, hit={"block": 2.0}
    )
    sch_id = r["schedule_id"]
    assert r["schedule"]["timeout"] == 0.1
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
        r = await _schedule(seeded_kernel, sid, "p_x", timeout=bad_value)
        assert r.get("reason") == "bad_arg" and r.get("arg") == "timeout", r
    listed = (await seeded_kernel.send([{"target": sid, "payload": {"type": "list"}}]))[
        0
    ]["result"]["schedules"]
    assert listed == []


async def test_non_int_args_are_bad_arg_refusals(seeded_kernel, store_agent):
    """#136 — a present-but-wrong number is a TYPO, not a new default. `int()`
    used to raise out of the handler and `or 60` used to swallow it; either way
    the caller's mistake became a silent behaviour change."""
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
    """#136 — `payload: [1]` used to raise `.get` out of the handler."""
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


async def test_a_non_numeric_tick_sec_is_refused_at_write_time(seeded_kernel):
    """#136 — `tick_sec: "0.5"` was silently defaulted to 1.0 (rust/swift) or
    raised inside the tick loop (python). The record is judged where the mistake
    is made."""
    r = (
        await seeded_kernel.send(
            [
                {
                    "target": "kernel_state",
                    "payload": {
                        "type": "create_agent",
                        "parent_id": "kernel_state",
                        "handler_module": "scheduler.tools",
                        "id": "sch_ticks",
                        "tick_sec": "0.5",
                    },
                }
            ]
        )
    )[0]["result"]
    assert r.get("reason") == "invalid_meta" and "tick_sec" in r.get("error", ""), r
    assert "sch_ticks" not in seeded_kernel.ctx.agents


# ─── verb answers, pinned (reduction candidate J: its single-item sends) ─────


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
