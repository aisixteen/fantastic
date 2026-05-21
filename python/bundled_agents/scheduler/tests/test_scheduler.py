"""scheduler bundle — recurring tasks."""

from __future__ import annotations


async def _make_scheduler(kernel, file_agent_id):
    rec = await kernel.send(
        "core",
        {
            "type": "create_agent",
            "handler_module": "scheduler.tools",
            "file_agent_id": file_agent_id,
        },
    )
    return rec["id"]


async def test_reflect_includes_file_agent_id(seeded_kernel, file_agent):
    sid = await _make_scheduler(seeded_kernel, file_agent)
    r = await seeded_kernel.send(sid, {"type": "reflect"})
    assert r["file_agent_id"] == file_agent
    assert "schedule" in r["verbs"]


async def test_boot_requires_file_agent_id(seeded_kernel):
    rec = await seeded_kernel.send(
        "core",
        {"type": "create_agent", "handler_module": "scheduler.tools"},
    )
    # boot was auto-called by core; explicit reboot should still fail
    r = await seeded_kernel.send(rec["id"], {"type": "boot"})
    assert "error" in r
    assert "file_agent_id" in r["error"]


async def test_schedule_requires_file_agent_id(seeded_kernel):
    rec = await seeded_kernel.send(
        "core",
        {"type": "create_agent", "handler_module": "scheduler.tools"},
    )
    r = await seeded_kernel.send(
        rec["id"],
        {
            "type": "schedule",
            "target": "cli",
            "payload": {"type": "say", "text": "x"},
            "interval_seconds": 1,
        },
    )
    assert "error" in r
    assert "file_agent_id" in r["error"]


async def test_schedule_persists_via_file_agent(seeded_kernel, file_agent, tmp_path):
    sid = await _make_scheduler(seeded_kernel, file_agent)
    r = await seeded_kernel.send(
        sid,
        {
            "type": "schedule",
            "target": "cli",
            "payload": {"type": "say", "text": "tick"},
            "interval_seconds": 60,
        },
    )
    assert "schedule_id" in r
    persisted = (
        tmp_path / ".fantastic" / "agents" / sid / "schedules.json"
    ).read_text()
    assert r["schedule_id"] in persisted


async def test_schedule_validation(seeded_kernel, file_agent):
    sid = await _make_scheduler(seeded_kernel, file_agent)
    no_target = await seeded_kernel.send(
        sid,
        {"type": "schedule", "payload": {"type": "x"}, "interval_seconds": 5},
    )
    assert "error" in no_target
    no_payload_type = await seeded_kernel.send(
        sid,
        {"type": "schedule", "target": "cli", "payload": {}, "interval_seconds": 5},
    )
    assert "error" in no_payload_type


async def test_unschedule(seeded_kernel, file_agent):
    sid = await _make_scheduler(seeded_kernel, file_agent)
    r = await seeded_kernel.send(
        sid,
        {
            "type": "schedule",
            "target": "cli",
            "payload": {"type": "say", "text": "x"},
            "interval_seconds": 60,
        },
    )
    sch_id = r["schedule_id"]
    out = await seeded_kernel.send(sid, {"type": "unschedule", "schedule_id": sch_id})
    assert out["removed"] is True


async def test_list(seeded_kernel, file_agent):
    sid = await _make_scheduler(seeded_kernel, file_agent)
    await seeded_kernel.send(
        sid,
        {
            "type": "schedule",
            "target": "cli",
            "payload": {"type": "say", "text": "x"},
            "interval_seconds": 60,
        },
    )
    r = await seeded_kernel.send(sid, {"type": "list"})
    assert len(r["schedules"]) == 1


async def test_pause_resume_one(seeded_kernel, file_agent):
    sid = await _make_scheduler(seeded_kernel, file_agent)
    r = await seeded_kernel.send(
        sid,
        {
            "type": "schedule",
            "target": "cli",
            "payload": {"type": "say", "text": "x"},
            "interval_seconds": 60,
        },
    )
    sch_id = r["schedule_id"]
    p = await seeded_kernel.send(sid, {"type": "pause", "schedule_id": sch_id})
    assert p["paused"] is True
    r = await seeded_kernel.send(sid, {"type": "resume", "schedule_id": sch_id})
    assert r["resumed"] is True


async def test_tick_now_fires(seeded_kernel, file_agent, tmp_path):
    sid = await _make_scheduler(seeded_kernel, file_agent)
    r = await seeded_kernel.send(
        sid,
        {
            "type": "schedule",
            "target": "cli",
            "payload": {"type": "say", "text": "x"},
            "interval_seconds": 60,
        },
    )
    sch_id = r["schedule_id"]
    out = await seeded_kernel.send(sid, {"type": "tick_now", "schedule_id": sch_id})
    assert out["fired"] is True
    # history.jsonl now exists
    hist_path = tmp_path / ".fantastic" / "agents" / sid / "history.jsonl"
    assert hist_path.exists()
    assert "schedule_fired" in hist_path.read_text()


async def test_history(seeded_kernel, file_agent):
    sid = await _make_scheduler(seeded_kernel, file_agent)
    r = await seeded_kernel.send(
        sid,
        {
            "type": "schedule",
            "target": "cli",
            "payload": {"type": "say", "text": "x"},
            "interval_seconds": 60,
        },
    )
    sch_id = r["schedule_id"]
    await seeded_kernel.send(sid, {"type": "tick_now", "schedule_id": sch_id})
    h = await seeded_kernel.send(sid, {"type": "history", "limit": 10})
    assert h["count"] >= 1


async def test_fire_emits_to_target_inbox(seeded_kernel, file_agent):
    sid = await _make_scheduler(seeded_kernel, file_agent)
    r = await seeded_kernel.send(
        sid,
        {
            "type": "schedule",
            "target": "cli",
            "payload": {"type": "say", "text": "x"},
            "interval_seconds": 60,
        },
    )
    sch_id = r["schedule_id"]
    await seeded_kernel.send(sid, {"type": "tick_now", "schedule_id": sch_id})
    # cli's inbox should have a schedule_fired event AND the original {say} payload
    q = seeded_kernel._ensure_inbox("cli")
    msgs = []
    while not q.empty():
        msgs.append(q.get_nowait())
    types = [m["type"] for m in msgs]
    assert "schedule_fired" in types
    assert "say" in types


async def test_unknown_verb_errors(seeded_kernel, file_agent):
    sid = await _make_scheduler(seeded_kernel, file_agent)
    r = await seeded_kernel.send(sid, {"type": "garbage"})
    assert "error" in r
