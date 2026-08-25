"""scheduler bundle — recurring tasks (persists THROUGH the loader)."""

from __future__ import annotations


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


async def test_reflect_no_longer_carries_file_bridge_id(seeded_kernel, store_agent):
    sid = await _make_scheduler(seeded_kernel)
    r = (await seeded_kernel.send([{"target": sid, "payload": {"type": "reflect"}}]))[
        0
    ]["result"]
    assert "file_bridge_id" not in r  # persists through the loader, no per-agent wiring
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
        tmp_path / ".fantastic" / "agents" / sid / "schedules.json"
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
    # history.jsonl now exists
    hist_path = tmp_path / ".fantastic" / "agents" / sid / "history.jsonl"
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
    (
        await seeded_kernel.send(
            [{"target": sid, "payload": {"type": "tick_now", "schedule_id": sch_id}}]
        )
    )[0]["result"]
    # ONE delivery per tick: the target gets the PAYLOAD and nothing else. The old
    # contract also echoed `schedule_fired` at the target, so a live program's
    # on_message saw every tick twice (measured 2026-08-07 — opus had to design a
    # filter around it). Telemetry rides the scheduler's OWN inbox + history.
    q = seeded_kernel.ctx.ensure_inbox("cli")
    msgs = []
    while not q.empty():
        msgs.append(q.get_nowait())
    types = [m["type"] for m in msgs]
    assert "say" in types
    assert "schedule_fired" not in types, "telemetry echoed at the target again"
    own = seeded_kernel.ctx.ensure_inbox(sid)
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
