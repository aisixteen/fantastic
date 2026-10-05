"""System verbs — create / update / delete / list / reflect.

System verbs are baked into the `Agent` class; every agent answers them
natively for its own children. These exercise them via the root (`kernel_state`,
now an `kernel_state` agent). Disk side effects (persist on create, rmtree
on delete) are the loader's job — covered in the kernel_state tests — so
these stay in-memory and assert the live-tree + event behavior.
"""

from __future__ import annotations


async def test_reflect_returns_uniform_identity(seeded_kernel):
    """Root reflect returns the uniform substrate identity + tree (default
    all), even though the root now carries the `kernel_state` handler_module.
    Transports/wire docs moved to the root readme; the bundle catalog is
    the opt-in `bundles` flag (not in a plain reflect)."""
    r = (
        await seeded_kernel.send(
            [{"target": "kernel_state", "payload": {"type": "reflect"}}]
        )
    )[0]["result"]
    assert r["sentence"].startswith("Fantastic kernel")
    assert r["tree"]["id"] == "kernel_state"
    assert "transports" not in r
    assert "available_bundles" not in r
    catalog = (
        await seeded_kernel.send(
            [
                {
                    "target": "kernel_state",
                    "payload": {"type": "reflect", "bundles": "all"},
                }
            ]
        )
    )[0]["result"]
    assert any(b["name"] == "file_bridge" for b in catalog["bundles"])


async def test_list_agents(seeded_kernel):
    r = (
        await seeded_kernel.send(
            [{"target": "kernel_state", "payload": {"type": "list_agents"}}]
        )
    )[0]["result"]
    ids = {a["id"] for a in r["agents"]}
    assert "kernel_state" in ids
    assert "cli" in ids


async def test_list_agents_points_back_at_the_briefing(seeded_kernel):
    """`list_agents` is a natural first call and must not be a dead end.

    Probe 06's two arms differed only in their opening call: `reflect` got
    `self_extension` and tried to author 10 times, `list_agents` got bare records
    and never tried at all. The pointer has to name the verb that carries the
    recipe AND the field, or a caller has no way from here to there.
    """
    r = (
        await seeded_kernel.send(
            [{"target": "kernel_state", "payload": {"type": "list_agents"}}]
        )
    )[0]["result"]
    more = r.get("more", "")
    assert "reflect" in more
    assert "self_extension" in more


async def test_create_agent_requires_handler_module(seeded_kernel):
    r = (
        await seeded_kernel.send(
            [{"target": "kernel_state", "payload": {"type": "create_agent"}}]
        )
    )[0]["result"]
    assert "error" in r


async def test_create_agent_emits_agent_created(seeded_kernel):
    seeded_kernel.ctx.ensure_inbox("watcher")  # a watcher registers first
    seeded_kernel.watch("kernel_state", "watcher")
    (
        await seeded_kernel.send(
            [
                {
                    "target": "kernel",
                    "payload": {
                        "type": "create_agent",
                        "parent_id": "kernel_state",
                        "handler_module": "file_bridge.tools",
                        "allow": ["."],
                    },
                }
            ]
        )
    )[0]["result"]
    q = seeded_kernel.ctx.ensure_inbox("watcher")
    events = []
    while not q.empty():
        events.append(q.get_nowait())
    types = {e["type"] for e in events}
    assert "agent_created" in types


async def test_agent_created_reaches_a_parent_at_any_depth(seeded_kernel):
    """The event lands on the PARENT's inbox at every depth (audit 2026-10-03,
    KV-4). It was emitted as the root, so the gate judged root→parent, and the
    root's baseline grant then reached depth 1 only: a parent two levels down
    never got it — an operator create included, an agent creating under
    itself too — and nothing reported the drop."""
    from _testkit import as_agent

    k = seeded_kernel
    k.create(None, id="kv4_a")
    k.ctx.agents["kv4_a"].create(None, id="kv4_b")  # depth 2
    k.ctx.update("kv4_b", can_call={"$self": "*", k.id: ["create_agent"]})
    k.ctx.update(k.id, callable_by={"$self": "*", "kv4_b": ["create_agent"]})
    q = k.ctx.ensure_inbox("kv4_b")

    async def create_under_b(cid: str) -> dict:
        return (
            await k.send(
                [
                    {
                        "target": "kernel",
                        "payload": {
                            "type": "create_agent",
                            "parent_id": "kv4_b",
                            "handler_module": "file_bridge.tools",
                            "allow": ["."],
                            "id": cid,
                        },
                    }
                ]
            )
        )[0]["result"]

    assert (await create_under_b("kv4_by_operator"))["id"] == "kv4_by_operator"
    with as_agent(k, "kv4_b"):  # granted create_agent: placement under itself
        assert (await create_under_b("kv4_by_self"))["id"] == "kv4_by_self"
    got = []
    while not q.empty():
        got.append(q.get_nowait())
    created = [e["id"] for e in got if e.get("type") == "agent_created"]
    assert created == ["kv4_by_operator", "kv4_by_self"], got


async def test_delete_agent(seeded_kernel):
    rec = (
        await seeded_kernel.send(
            [
                {
                    "target": "kernel",
                    "payload": {
                        "type": "create_agent",
                        "parent_id": "kernel_state",
                        "handler_module": "file_bridge.tools",
                        "allow": ["."],
                    },
                }
            ]
        )
    )[0]["result"]
    aid = rec["id"]
    r = (
        await seeded_kernel.send(
            [{"target": "kernel_state", "payload": {"type": "delete_agent", "id": aid}}]
        )
    )[0]["result"]
    assert r["deleted"] is True


async def test_delete_agent_emits_event(seeded_kernel):
    rec = (
        await seeded_kernel.send(
            [
                {
                    "target": "kernel",
                    "payload": {
                        "type": "create_agent",
                        "parent_id": "kernel_state",
                        "handler_module": "file_bridge.tools",
                        "allow": ["."],
                    },
                }
            ]
        )
    )[0]["result"]
    seeded_kernel.ctx.ensure_inbox("w")
    seeded_kernel.watch("kernel_state", "w")
    (
        await seeded_kernel.send(
            [
                {
                    "target": "kernel_state",
                    "payload": {"type": "delete_agent", "id": rec["id"]},
                }
            ]
        )
    )[0]["result"]
    q = seeded_kernel.ctx.ensure_inbox("w")
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
    rec = (
        await seeded_kernel.send(
            [
                {
                    "target": "kernel",
                    "payload": {
                        "type": "create_agent",
                        "parent_id": "kernel_state",
                        "handler_module": "file_bridge.tools",
                        "allow": ["."],
                    },
                }
            ]
        )
    )[0]["result"]
    aid = rec["id"]
    assert aid in seeded_kernel.ctx.agents
    r = (
        await seeded_kernel.send(
            [{"target": "kernel_state", "payload": {"type": "delete_agent", "id": aid}}]
        )
    )[0]["result"]
    assert r["deleted"] is True
    assert seeded_kernel.get(aid) is None
    assert aid not in seeded_kernel.ctx.agents
    assert aid not in seeded_kernel._children


async def test_delete_agent_handler_module_without_on_delete(seeded_kernel):
    """Bundles that don't define `on_delete` fall through cleanly (no
    error). file_bridge.tools has no on_delete hook."""
    rec = (
        await seeded_kernel.send(
            [
                {
                    "target": "kernel",
                    "payload": {
                        "type": "create_agent",
                        "parent_id": "kernel_state",
                        "handler_module": "file_bridge.tools",
                        "allow": ["."],
                    },
                }
            ]
        )
    )[0]["result"]
    r = (
        await seeded_kernel.send(
            [
                {
                    "target": "kernel_state",
                    "payload": {"type": "delete_agent", "id": rec["id"]},
                }
            ]
        )
    )[0]["result"]
    assert r == {"deleted": True, "id": rec["id"]}
    assert seeded_kernel.get(rec["id"]) is None


async def test_update_agent(seeded_kernel):
    rec = (
        await seeded_kernel.send(
            [
                {
                    "target": "kernel",
                    "payload": {
                        "type": "create_agent",
                        "parent_id": "kernel_state",
                        "handler_module": "file_bridge.tools",
                        "allow": ["."],
                    },
                }
            ]
        )
    )[0]["result"]
    r = (
        await seeded_kernel.send(
            [
                {
                    "target": "kernel_state",
                    "payload": {
                        "type": "update_agent",
                        "id": rec["id"],
                        "x": 100,
                        "y": 200,
                    },
                }
            ]
        )
    )[0]["result"]
    assert r["updated"] is True
    refreshed = seeded_kernel.get(rec["id"])
    assert refreshed["x"] == 100
    assert refreshed["y"] == 200


async def test_update_agent_emits_event_with_changed_keys(seeded_kernel):
    rec = (
        await seeded_kernel.send(
            [
                {
                    "target": "kernel",
                    "payload": {
                        "type": "create_agent",
                        "parent_id": "kernel_state",
                        "handler_module": "file_bridge.tools",
                        "allow": ["."],
                    },
                }
            ]
        )
    )[0]["result"]
    seeded_kernel.ctx.ensure_inbox("w")
    seeded_kernel.watch("kernel_state", "w")
    # drain creation events
    q = seeded_kernel.ctx.ensure_inbox("w")
    while not q.empty():
        q.get_nowait()
    (
        await seeded_kernel.send(
            [
                {
                    "target": "kernel_state",
                    "payload": {"type": "update_agent", "id": rec["id"], "model": "x"},
                }
            ]
        )
    )[0]["result"]
    events = []
    while not q.empty():
        events.append(q.get_nowait())
    assert any(e["type"] == "agent_updated" and "model" in e["changed"] for e in events)


async def test_delete_agent_refused_when_locked(seeded_kernel):
    """delete_lock=True on the record blocks delete. Response carries
    explicit `locked:True` flag for machine-readable detection (LLM
    callers parse this from their tool reply)."""
    rec = (
        await seeded_kernel.send(
            [
                {
                    "target": "kernel_state",
                    "payload": {
                        "type": "create_agent",
                        "parent_id": "kernel_state",
                        "handler_module": "file_bridge.tools",
                        "allow": ["."],
                        "delete_lock": True,
                    },
                }
            ]
        )
    )[0]["result"]
    aid = rec["id"]
    r = (
        await seeded_kernel.send(
            [{"target": "kernel_state", "payload": {"type": "delete_agent", "id": aid}}]
        )
    )[0]["result"]
    assert r.get("reason") == "locked"
    assert r.get("id") == aid
    assert "delete_lock" in r.get("error", "")
    # Record still present.
    assert seeded_kernel.get(aid) is not None


async def test_delete_agent_succeeds_after_unlock(seeded_kernel):
    rec = (
        await seeded_kernel.send(
            [
                {
                    "target": "kernel_state",
                    "payload": {
                        "type": "create_agent",
                        "parent_id": "kernel_state",
                        "handler_module": "file_bridge.tools",
                        "allow": ["."],
                        "delete_lock": True,
                    },
                }
            ]
        )
    )[0]["result"]
    aid = rec["id"]
    # Locked → refused.
    r1 = (
        await seeded_kernel.send(
            [{"target": "kernel_state", "payload": {"type": "delete_agent", "id": aid}}]
        )
    )[0]["result"]
    assert r1.get("reason") == "locked"
    # Unlock via update_agent.
    (
        await seeded_kernel.send(
            [
                {
                    "target": "kernel_state",
                    "payload": {
                        "type": "update_agent",
                        "id": aid,
                        "delete_lock": False,
                    },
                }
            ]
        )
    )[0]["result"]
    # Now delete succeeds.
    r2 = (
        await seeded_kernel.send(
            [{"target": "kernel_state", "payload": {"type": "delete_agent", "id": aid}}]
        )
    )[0]["result"]
    assert r2["deleted"] is True
    assert seeded_kernel.get(aid) is None


async def test_delete_agent_unlocked_record_works(seeded_kernel):
    """Records without delete_lock (or False) delete normally."""
    rec = (
        await seeded_kernel.send(
            [
                {
                    "target": "kernel",
                    "payload": {
                        "type": "create_agent",
                        "parent_id": "kernel_state",
                        "handler_module": "file_bridge.tools",
                        "allow": ["."],
                    },
                }
            ]
        )
    )[0]["result"]
    r = (
        await seeded_kernel.send(
            [
                {
                    "target": "kernel_state",
                    "payload": {"type": "delete_agent", "id": rec["id"]},
                }
            ]
        )
    )[0]["result"]
    assert r["deleted"] is True
    assert "locked" not in r
