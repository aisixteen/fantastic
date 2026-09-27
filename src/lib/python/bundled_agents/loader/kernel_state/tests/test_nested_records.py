"""A SESSION loader (a `kernel_state` with a `root`, under a ws leg) persists a
federated kernel's records — a canvas and the thin agents nested under it. Those
records are never live on the host, so the record-owner walk and the path
resolution climb THIS LOADER'S persisted records (disk position) past the live
tree: nested 2+ levels is allowed for the owning leg and refused for a sibling,
and an existing record is placed where it is — never by the payload's parent."""

from __future__ import annotations

from pathlib import Path

from kernel import sender_context

ROOT = ".fantastic/web"


async def _call(k, target, payload):
    return (await k.send([{"target": target, "payload": payload}]))[0]["result"]


async def _setup(k):
    for rec in (
        {"id": "wws", "handler_module": "file_bridge.tools", "parent_id": k.id},
        {
            "id": "rest_leg",
            "handler_module": "file_bridge.tools",
            "parent_id": k.id,
            "can_call": {"web_loader": "*"},
        },
        {
            "id": "web_loader",
            "handler_module": "kernel_state.tools",
            "parent_id": "wws",
            "root": ROOT,
            "watch": False,
            "callable_by": {"rest_leg": "*"},
        },
    ):
        out = await _call(k, k.id, {"type": "create_agent", **rec})
        assert out.get("id") == rec["id"], out


async def _persist(k, who, rid, parent):
    with sender_context(who):
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


async def test_a_sibling_may_not_touch_the_nested_records(seeded_kernel):
    k = seeded_kernel
    await _setup(k)
    await _persist(k, "wws", "canvas", "web_loader")
    await _persist(k, "wws", "terminal_view", "canvas")
    r = await _persist(k, "rest_leg", "sneak", "terminal_view")
    assert r.get("reason") == "unauthorized", r
    with sender_context("rest_leg"):
        r = await _call(
            k,
            "web_loader",
            {"type": "forget_record", "id": "terminal_view", "parent_id": "canvas"},
        )
    assert r.get("reason") == "unauthorized", r
    assert Path(ROOT, "agents/canvas/agents/terminal_view/agent.yaml").exists()


async def test_an_existing_record_is_placed_by_its_position_not_the_payload(
    seeded_kernel,
):
    """Naming a record that exists with `parent_id` = yourself used to pass the
    guard (the payload's parent was walked first) and overwrite it."""
    k = seeded_kernel
    await _setup(k)
    await _persist(k, "wws", "canvas", "web_loader")
    await _persist(k, "wws", "terminal_view", "canvas")
    r = await _persist(k, "rest_leg", "terminal_view", "rest_leg")
    assert r.get("reason") == "unauthorized", r
    assert not Path(ROOT, "agents/terminal_view").exists()


async def test_forget_record_removes_a_nested_record_where_it_is(seeded_kernel):
    k = seeded_kernel
    await _setup(k)
    await _persist(k, "wws", "canvas", "web_loader")
    await _persist(k, "wws", "terminal_view", "canvas")
    await _persist(k, "wws", "deep", "terminal_view")
    with sender_context("wws"):
        r = await _call(k, "web_loader", {"type": "forget_record", "id": "deep"})
    assert r == {"ok": True}, r
    assert not Path(ROOT, "agents/canvas/agents/terminal_view/agents/deep").exists()
    assert Path(ROOT, "agents/canvas/agents/terminal_view/agent.yaml").exists()


async def test_an_id_in_neither_tree_nor_disk_is_placed_by_the_payload_and_guarded(
    seeded_kernel,
):
    """No fallback: a brand-new record's placement comes from the payload, and the
    walk from there must still reach the caller."""
    k = seeded_kernel
    await _setup(k)
    r = await _persist(k, "wws", "orphan", "nowhere")
    assert r.get("reason") == "unauthorized", r
