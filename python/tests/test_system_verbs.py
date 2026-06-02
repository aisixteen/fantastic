"""System verbs — create / update / delete / list / reflect.

System verbs are baked into the `Agent` class; every agent answers them
natively for its own children. These exercise them via the root (`fs_loader`,
now an `fs_loader` agent). Disk side effects (persist on create, rmtree
on delete) are the loader's job — covered in the fs_loader tests — so
these stay in-memory and assert the live-tree + event behavior.
"""

from __future__ import annotations


async def test_reflect_returns_uniform_identity(seeded_kernel):
    """Root reflect returns the uniform substrate identity + tree (default
    all), even though the root now carries the `fs_loader` handler_module.
    Transports/wire docs moved to the root readme; the bundle catalog is
    the opt-in `bundles` flag (not in a plain reflect)."""
    r = await seeded_kernel.send("fs_loader", {"type": "reflect"})
    assert r["sentence"].startswith("Fantastic kernel")
    assert r["tree"]["id"] == "fs_loader"
    assert "transports" not in r
    assert "available_bundles" not in r
    catalog = await seeded_kernel.send(
        "fs_loader", {"type": "reflect", "bundles": "all"}
    )
    assert any(b["name"] == "file" for b in catalog["bundles"])


async def test_list_agents(seeded_kernel):
    r = await seeded_kernel.send("fs_loader", {"type": "list_agents"})
    ids = {a["id"] for a in r["agents"]}
    assert "fs_loader" in ids
    assert "cli" in ids


async def test_create_agent_requires_handler_module(seeded_kernel):
    r = await seeded_kernel.send("fs_loader", {"type": "create_agent"})
    assert "error" in r


async def test_create_agent_emits_agent_created(seeded_kernel):
    seeded_kernel.watch("fs_loader", "watcher")
    seeded_kernel._ensure_inbox("watcher")
    await seeded_kernel.send(
        "fs_loader",
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
        "fs_loader",
        {"type": "create_agent", "handler_module": "file.tools"},
    )
    aid = rec["id"]
    r = await seeded_kernel.send("fs_loader", {"type": "delete_agent", "id": aid})
    assert r["deleted"] is True


async def test_delete_agent_emits_event(seeded_kernel):
    rec = await seeded_kernel.send(
        "fs_loader",
        {"type": "create_agent", "handler_module": "file.tools"},
    )
    seeded_kernel._ensure_inbox("w")
    seeded_kernel.watch("fs_loader", "w")
    await seeded_kernel.send("fs_loader", {"type": "delete_agent", "id": rec["id"]})
    q = seeded_kernel._ensure_inbox("w")
    events = []
    while not q.empty():
        events.append(q.get_nowait())
    assert any(e["type"] == "agent_deleted" for e in events)


async def test_delete_agent_runs_on_delete_hook(seeded_kernel):
    """delete_agent runs each agent's `on_delete` hook depth-first BEFORE
    detaching the record from `ctx.agents` + the parent's `_children`.
    The hook tears down bundle process-memory state; disk cleanup is the
    loader's (the `removed` event drives rmtree). Here we verify the
    cascade detaches the record cleanly (no error, gone from the tree)."""
    rec = await seeded_kernel.send(
        "fs_loader",
        {"type": "create_agent", "handler_module": "file.tools"},
    )
    aid = rec["id"]
    assert aid in seeded_kernel.ctx.agents
    r = await seeded_kernel.send("fs_loader", {"type": "delete_agent", "id": aid})
    assert r["deleted"] is True
    assert seeded_kernel.get(aid) is None
    assert aid not in seeded_kernel.ctx.agents
    assert aid not in seeded_kernel._children


async def test_delete_agent_handler_module_without_on_delete(seeded_kernel):
    """Bundles that don't define `on_delete` fall through cleanly (no
    error). file.tools has no on_delete hook."""
    rec = await seeded_kernel.send(
        "fs_loader",
        {"type": "create_agent", "handler_module": "file.tools"},
    )
    r = await seeded_kernel.send("fs_loader", {"type": "delete_agent", "id": rec["id"]})
    assert r == {"deleted": True, "id": rec["id"]}
    assert seeded_kernel.get(rec["id"]) is None


async def test_update_agent(seeded_kernel):
    rec = await seeded_kernel.send(
        "fs_loader",
        {"type": "create_agent", "handler_module": "file.tools"},
    )
    r = await seeded_kernel.send(
        "fs_loader",
        {"type": "update_agent", "id": rec["id"], "x": 100, "y": 200},
    )
    assert r["updated"] is True
    refreshed = seeded_kernel.get(rec["id"])
    assert refreshed["x"] == 100
    assert refreshed["y"] == 200


async def test_update_agent_emits_event_with_changed_keys(seeded_kernel):
    rec = await seeded_kernel.send(
        "fs_loader",
        {"type": "create_agent", "handler_module": "file.tools"},
    )
    seeded_kernel._ensure_inbox("w")
    seeded_kernel.watch("fs_loader", "w")
    # drain creation events
    q = seeded_kernel._ensure_inbox("w")
    while not q.empty():
        q.get_nowait()
    await seeded_kernel.send(
        "fs_loader",
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
        "fs_loader",
        {"type": "create_agent", "handler_module": "file.tools", "delete_lock": True},
    )
    aid = rec["id"]
    r = await seeded_kernel.send("fs_loader", {"type": "delete_agent", "id": aid})
    assert r.get("locked") is True
    assert r.get("id") == aid
    assert "delete_lock" in r.get("error", "")
    # Record still present.
    assert seeded_kernel.get(aid) is not None


async def test_delete_agent_succeeds_after_unlock(seeded_kernel):
    rec = await seeded_kernel.send(
        "fs_loader",
        {"type": "create_agent", "handler_module": "file.tools", "delete_lock": True},
    )
    aid = rec["id"]
    # Locked → refused.
    r1 = await seeded_kernel.send("fs_loader", {"type": "delete_agent", "id": aid})
    assert r1.get("locked") is True
    # Unlock via update_agent.
    await seeded_kernel.send(
        "fs_loader", {"type": "update_agent", "id": aid, "delete_lock": False}
    )
    # Now delete succeeds.
    r2 = await seeded_kernel.send("fs_loader", {"type": "delete_agent", "id": aid})
    assert r2["deleted"] is True
    assert seeded_kernel.get(aid) is None


async def test_delete_agent_unlocked_record_works(seeded_kernel):
    """Records without delete_lock (or False) delete normally."""
    rec = await seeded_kernel.send(
        "fs_loader",
        {"type": "create_agent", "handler_module": "file.tools"},
    )
    r = await seeded_kernel.send("fs_loader", {"type": "delete_agent", "id": rec["id"]})
    assert r["deleted"] is True
    assert "locked" not in r
