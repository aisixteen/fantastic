"""A SESSION loader (a `kernel_state` with a `root`, under a ws leg) persists a
federated kernel's records — a canvas and the thin agents nested under it. Those
records are never live on the host, so the path resolution climbs THIS LOADER'S
persisted records (disk position) past the live tree: nested 2+ levels lands under
its real parents, and an existing record is placed where it is — never by the
payload's parent. WHO may write is the grant alone: any agent
granted the record verbs on the loader writes any record."""

from __future__ import annotations

from pathlib import Path

from _testkit import as_agent

ROOT = ".fantastic/web"


async def _call(k, target, payload):
    return (await k.send([{"target": target, "payload": payload}]))[0]["result"]


async def _setup(k):
    for rec in (
        {
            "id": "wws",
            "handler_module": "file_bridge.tools",
            "allow": ["."],
            "parent_id": k.id,
            "can_call": {
                "web_loader": ["persist_record", "forget_record", "load_tree"]
            },
        },
        {
            "id": "rest_leg",
            "handler_module": "file_bridge.tools",
            "allow": ["."],
            "parent_id": k.id,
            "can_call": {"web_loader": "*"},
        },
        {
            "id": "web_loader",
            "handler_module": "kernel_state.tools",
            "parent_id": "wws",
            "root": ROOT,
            "watch": False,
            "callable_by": {"rest_leg": "*", "wws": "*"},
        },
    ):
        out = await _call(k, k.id, {"type": "create_agent", **rec})
        assert out.get("id") == rec["id"], out


async def _persist(k, who, rid, parent):
    with as_agent(k, who):
        return await _call(
            k,
            "web_loader",
            {"type": "persist_record", "record": {"id": rid, "parent_id": parent}},
        )


async def test_the_owning_leg_persists_records_nested_under_a_federated_record(
    seeded_kernel,
):
    k = seeded_kernel
    await _setup(k)
    assert (await _persist(k, "wws", "canvas", "web_loader")) == {"ok": True}
    assert (await _persist(k, "wws", "terminal_view", "canvas")) == {"ok": True}
    assert (await _persist(k, "wws", "deep", "terminal_view")) == {"ok": True}
    assert Path(
        ROOT, "agents/canvas/agents/terminal_view/agents/deep/agent.yaml"
    ).exists(), "a 2-level nested record lands under its real parents"
    tree = await _call(k, "web_loader", {"type": "load_tree"})
    parents = {r["id"]: r.get("parent_id") for r in tree["records"]}
    assert parents["terminal_view"] == "canvas" and parents["deep"] == "terminal_view"


async def test_a_granted_sibling_writes_the_nested_records(seeded_kernel):
    """The grant is the whole rule: `rest_leg` holds `*` on the loader, so it writes
    under another leg's records and forgets them."""
    k = seeded_kernel
    await _setup(k)
    await _persist(k, "wws", "canvas", "web_loader")
    await _persist(k, "wws", "terminal_view", "canvas")
    assert (await _persist(k, "rest_leg", "extra", "terminal_view")) == {"ok": True}
    assert Path(
        ROOT, "agents/canvas/agents/terminal_view/agents/extra/agent.yaml"
    ).exists()
    with as_agent(seeded_kernel, "rest_leg"):
        r = await _call(
            k,
            "web_loader",
            {"type": "forget_record", "id": "terminal_view", "parent_id": "canvas"},
        )
    assert r == {"ok": True}, r
    assert not Path(ROOT, "agents/canvas/agents/terminal_view").exists()


async def test_an_existing_record_is_placed_by_its_position_not_the_payload(
    seeded_kernel,
):
    """An existing record is written where it IS, whatever `parent_id` the payload
    names."""
    k = seeded_kernel
    await _setup(k)
    await _persist(k, "wws", "canvas", "web_loader")
    await _persist(k, "wws", "terminal_view", "canvas")
    assert (await _persist(k, "rest_leg", "terminal_view", "rest_leg")) == {"ok": True}
    assert not Path(ROOT, "agents/terminal_view").exists()
    assert Path(ROOT, "agents/canvas/agents/terminal_view/agent.yaml").exists()


async def test_forget_record_removes_a_nested_record_where_it_is(seeded_kernel):
    k = seeded_kernel
    await _setup(k)
    await _persist(k, "wws", "canvas", "web_loader")
    await _persist(k, "wws", "terminal_view", "canvas")
    await _persist(k, "wws", "deep", "terminal_view")
    with as_agent(seeded_kernel, "wws"):
        r = await _call(k, "web_loader", {"type": "forget_record", "id": "deep"})
    assert r == {"ok": True}, r
    assert not Path(ROOT, "agents/canvas/agents/terminal_view/agents/deep").exists()
    assert Path(ROOT, "agents/canvas/agents/terminal_view/agent.yaml").exists()


async def test_an_id_in_neither_tree_nor_disk_is_placed_by_the_payload(seeded_kernel):
    """No fallback: a brand-new record's placement comes from the payload."""
    k = seeded_kernel
    await _setup(k)
    assert (await _persist(k, "wws", "orphan", "nowhere")) == {"ok": True}
    assert Path(ROOT, "agents/nowhere/agents/orphan/agent.yaml").exists()
