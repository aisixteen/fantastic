"""core bundle — system verbs."""

from __future__ import annotations


async def test_reflect_returns_substrate_primer(seeded_kernel):
    """Root reflect returns the substrate primer — admin verbs are
    baked into Agent class itself and answered for any agent. The
    primer surfaces transports + tree + bundles for discovery."""
    r = await seeded_kernel.send("core", {"type": "reflect"})
    assert r["sentence"].startswith("Fantastic kernel")
    assert "transports" in r
    assert "tree" in r
    assert "available_bundles" in r


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


async def test_delete_agent_calls_on_delete_hook(seeded_kernel, tmp_path):
    """delete_agent calls each agent's `on_delete` hook depth-first
    BEFORE removing the record. The default `on_delete` rmtrees the
    agent's directory; bundles override with `async def on_delete(agent)`
    in their tools.py to tear down process-memory state (PTY children,
    uvicorn, etc.)."""
    rec = await seeded_kernel.send(
        "core",
        {"type": "create_agent", "handler_module": "file.tools"},
    )
    aid = rec["id"]
    agent_dir = seeded_kernel.ctx.agents[aid]._root_path
    assert agent_dir.exists(), f"agent dir should exist pre-delete: {agent_dir}"
    r = await seeded_kernel.send("core", {"type": "delete_agent", "id": aid})
    assert r["deleted"] is True
    # on_delete's default behavior is to rmtree the agent's dir.
    assert not agent_dir.exists(), f"agent dir should be gone: {agent_dir}"


async def test_delete_agent_handler_module_without_on_delete(seeded_kernel):
    """Bundles that don't define `on_delete` fall through to the
    default behavior (rmtree own dir). No error."""
    # file.tools doesn't define on_delete; delete should succeed via
    # the default rmtree path.
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
