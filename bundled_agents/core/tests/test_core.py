"""core bundle — system verbs."""

from __future__ import annotations


async def test_reflect_returns_verbs(seeded_kernel):
    r = await seeded_kernel.send("core", {"type": "reflect"})
    assert r["sentence"].startswith("Core")
    for v in (
        "list_agents",
        "create_agent",
        "delete_agent",
        "update_agent",
        "boot",
        "reflect",
    ):
        assert v in r["verbs"]


async def test_list_agents(seeded_kernel):
    r = await seeded_kernel.send("core", {"type": "list_agents"})
    ids = {a["id"] for a in r["agents"]}
    assert "core" in ids
    assert "cli" in ids


async def test_create_agent_requires_handler_module(seeded_kernel):
    r = await seeded_kernel.send("core", {"type": "create_agent"})
    assert "error" in r


async def test_create_agent_emits_agent_created(seeded_kernel):
    seeded_kernel.watch("core", "watcher")
    seeded_kernel._ensure_inbox("watcher")
    await seeded_kernel.send(
        "core",
        {"type": "create_agent", "handler_module": "file.tools"},
    )
    q = seeded_kernel._ensure_inbox("watcher")
    events = []
    while not q.empty():
        events.append(q.get_nowait())
    types = {e["type"] for e in events}
    assert "agent_created" in types


async def test_delete_agent(seeded_kernel):
    rec = await seeded_kernel.send(
        "core",
        {"type": "create_agent", "handler_module": "file.tools"},
    )
    aid = rec["id"]
    r = await seeded_kernel.send("core", {"type": "delete_agent", "id": aid})
    assert r["deleted"] is True


async def test_delete_agent_emits_event(seeded_kernel):
    rec = await seeded_kernel.send(
        "core",
        {"type": "create_agent", "handler_module": "file.tools"},
    )
    seeded_kernel._ensure_inbox("w")
    seeded_kernel.watch("core", "w")
    await seeded_kernel.send("core", {"type": "delete_agent", "id": rec["id"]})
    q = seeded_kernel._ensure_inbox("w")
    events = []
    while not q.empty():
        events.append(q.get_nowait())
    assert any(e["type"] == "agent_deleted" for e in events)


async def test_delete_agent_sends_shutdown_to_target(seeded_kernel):
    """delete_agent must dispatch `shutdown` to the agent BEFORE
    removing its record, symmetric to create_agent's `boot`. This is
    how bundles tear down process-memory state (PTY children, uvicorn,
    etc.) — without it, orphan subprocesses outlive their record and
    keep emitting traffic to a dead inbox.

    We verify by watching the agent's inbox: the shutdown payload
    lands there before kernel.delete drops the inbox.
    """
    rec = await seeded_kernel.send(
        "core",
        {"type": "create_agent", "handler_module": "file.tools"},
    )
    aid = rec["id"]
    seen: list[dict] = []
    seeded_kernel._ensure_inbox("w")
    seeded_kernel.watch(aid, "w")
    await seeded_kernel.send("core", {"type": "delete_agent", "id": aid})
    q = seeded_kernel._ensure_inbox("w")
    while not q.empty():
        seen.append(q.get_nowait())
    types = [e.get("type") for e in seen]
    assert "shutdown" in types, (
        f"expected shutdown to be sent before delete; got {types}"
    )


async def test_delete_agent_ignores_unknown_shutdown_verb(seeded_kernel):
    """Bundles that don't implement `shutdown` return an unknown-verb
    error from their handler. delete_agent must not abort on that —
    it's an opt-in lifecycle hook, not a contract."""
    # file.tools doesn't implement `shutdown`. Delete should still
    # succeed end-to-end without raising.
    rec = await seeded_kernel.send(
        "core",
        {"type": "create_agent", "handler_module": "file.tools"},
    )
    r = await seeded_kernel.send("core", {"type": "delete_agent", "id": rec["id"]})
    assert r == {"deleted": True, "id": rec["id"]}
    assert seeded_kernel.get(rec["id"]) is None


async def test_update_agent(seeded_kernel):
    rec = await seeded_kernel.send(
        "core",
        {"type": "create_agent", "handler_module": "file.tools"},
    )
    r = await seeded_kernel.send(
        "core",
        {"type": "update_agent", "id": rec["id"], "x": 100, "y": 200},
    )
    assert r["updated"] is True
    refreshed = seeded_kernel.get(rec["id"])
    assert refreshed["x"] == 100
    assert refreshed["y"] == 200


async def test_update_agent_emits_event_with_changed_keys(seeded_kernel):
    rec = await seeded_kernel.send(
        "core",
        {"type": "create_agent", "handler_module": "file.tools"},
    )
    seeded_kernel._ensure_inbox("w")
    seeded_kernel.watch("core", "w")
    # drain creation events
    q = seeded_kernel._ensure_inbox("w")
    while not q.empty():
        q.get_nowait()
    await seeded_kernel.send(
        "core",
        {"type": "update_agent", "id": rec["id"], "model": "x"},
    )
    events = []
    while not q.empty():
        events.append(q.get_nowait())
    assert any(e["type"] == "agent_updated" and "model" in e["changed"] for e in events)


async def test_delete_agent_refused_when_locked(seeded_kernel):
    """delete_lock=True on the record blocks delete. Response carries
    explicit `locked:True` flag for machine-readable detection (LLM
    callers parse this from their tool reply)."""
    rec = await seeded_kernel.send(
        "core",
        {"type": "create_agent", "handler_module": "file.tools", "delete_lock": True},
    )
    aid = rec["id"]
    r = await seeded_kernel.send("core", {"type": "delete_agent", "id": aid})
    assert r.get("locked") is True
    assert r.get("id") == aid
    assert "delete_lock" in r.get("error", "")
    # Record still present.
    assert seeded_kernel.get(aid) is not None


async def test_delete_agent_succeeds_after_unlock(seeded_kernel):
    rec = await seeded_kernel.send(
        "core",
        {"type": "create_agent", "handler_module": "file.tools", "delete_lock": True},
    )
    aid = rec["id"]
    # Locked → refused.
    r1 = await seeded_kernel.send("core", {"type": "delete_agent", "id": aid})
    assert r1.get("locked") is True
    # Unlock via update_agent.
    await seeded_kernel.send(
        "core", {"type": "update_agent", "id": aid, "delete_lock": False}
    )
    # Now delete succeeds.
    r2 = await seeded_kernel.send("core", {"type": "delete_agent", "id": aid})
    assert r2["deleted"] is True
    assert seeded_kernel.get(aid) is None


async def test_delete_agent_unlocked_record_works(seeded_kernel):
    """Records without delete_lock (or False) delete normally."""
    rec = await seeded_kernel.send(
        "core",
        {"type": "create_agent", "handler_module": "file.tools"},
    )
    r = await seeded_kernel.send("core", {"type": "delete_agent", "id": rec["id"]})
    assert r["deleted"] is True
    assert "locked" not in r
