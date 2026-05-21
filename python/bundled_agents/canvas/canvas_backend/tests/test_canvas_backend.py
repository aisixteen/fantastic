"""canvas_backend — spatial discovery + structural membership.

In the recursive Agent model, members are this canvas's children
(structural). `add_agent` takes `handler_module` and creates a new
member as a child via `agent.create`. `remove_agent` cascades the
deletion through the substrate. The children dict IS the membership
list.
"""

from __future__ import annotations

from canvas_backend.tools import _intersects, _rect


async def _make_canvas_backend(kernel):
    rec = await kernel.send(
        "core",
        {"type": "create_agent", "handler_module": "canvas_backend.tools"},
    )
    return rec["id"]


# ─── pure helpers ───────────────────────────────────────────────


def test_intersects_overlapping():
    assert _intersects((0, 0, 10, 10), (5, 5, 10, 10))


def test_intersects_disjoint():
    assert not _intersects((0, 0, 10, 10), (100, 100, 10, 10))


def test_intersects_edge_touch_disjoint():
    assert not _intersects((0, 0, 10, 10), (11, 0, 10, 10))


def test_rect_uses_defaults():
    assert _rect({}) == (0.0, 0.0, 320.0, 220.0)


def test_rect_uses_record_values():
    assert _rect({"x": 5, "y": 6, "width": 100, "height": 200}) == (
        5.0,
        6.0,
        100.0,
        200.0,
    )


# ─── reflect / discover ─────────────────────────────────────────


async def test_reflect(seeded_kernel):
    cid = await _make_canvas_backend(seeded_kernel)
    r = await seeded_kernel.send(cid, {"type": "reflect"})
    assert r["sentence"].startswith("Spatial canvas")
    assert "discover" in r["verbs"]


async def test_discover_requires_positive_w_h(seeded_kernel):
    cid = await _make_canvas_backend(seeded_kernel)
    r = await seeded_kernel.send(
        cid, {"type": "discover", "x": 0, "y": 0, "w": 0, "h": 0}
    )
    assert "error" in r


async def test_discover_returns_intersecting_members(seeded_kernel):
    """Discover walks ONLY this canvas's direct children."""
    cid = await _make_canvas_backend(seeded_kernel)
    r = await seeded_kernel.send(
        cid,
        {
            "type": "add_agent",
            "handler_module": "html_agent.tools",
            "x": 100,
            "y": 100,
            "width": 50,
            "height": 50,
        },
    )
    assert r.get("ok") is True
    member_id = r["member_id"]
    r = await seeded_kernel.send(
        cid, {"type": "discover", "x": 0, "y": 0, "w": 200, "h": 200}
    )
    ids = {x["id"] for x in r["agents"]}
    assert member_id in ids


async def test_discover_excludes_self(seeded_kernel):
    cid = await _make_canvas_backend(seeded_kernel)
    r = await seeded_kernel.send(
        cid, {"type": "discover", "x": 0, "y": 0, "w": 10000, "h": 10000}
    )
    ids = {x["id"] for x in r["agents"]}
    assert cid not in ids


async def test_unknown_verb_errors(seeded_kernel):
    cid = await _make_canvas_backend(seeded_kernel)
    r = await seeded_kernel.send(cid, {"type": "garbage"})
    assert "error" in r


# ─── structural membership ──────────────────────────────────────


async def test_list_members_empty_by_default(seeded_kernel):
    cid = await _make_canvas_backend(seeded_kernel)
    r = await seeded_kernel.send(cid, {"type": "list_members"})
    assert r == {"members": []}


async def test_add_agent_appends(seeded_kernel):
    """add_agent creates a new member as the canvas's child."""
    cid = await _make_canvas_backend(seeded_kernel)
    r = await seeded_kernel.send(
        cid, {"type": "add_agent", "handler_module": "html_agent.tools"}
    )
    assert r.get("ok") is True
    member_id = r["member_id"]
    assert r["members"] == [member_id]
    # Persisted as a child of canvas.
    canvas_agent = seeded_kernel.ctx.agents[cid]
    assert member_id in canvas_agent._children


async def test_add_agent_emits_members_updated(seeded_kernel):
    cid = await _make_canvas_backend(seeded_kernel)
    events: list[dict] = []
    unsub = seeded_kernel.add_state_subscriber(events.append)
    try:
        await seeded_kernel.send(
            cid, {"type": "add_agent", "handler_module": "html_agent.tools"}
        )
    finally:
        unsub()
    member_events = [
        e["payload"]
        for e in events
        if e.get("kind") == "emit"
        and e.get("agent_id") == cid
        and e.get("payload", {}).get("type") == "members_updated"
    ]
    assert len(member_events) == 1
    assert len(member_events[0]["members"]) == 1


async def test_add_agent_refuses_missing_handler_module(seeded_kernel):
    cid = await _make_canvas_backend(seeded_kernel)
    r = await seeded_kernel.send(cid, {"type": "add_agent"})
    assert "error" in r
    assert "handler_module" in r["error"]


async def test_add_agent_refuses_non_renderable(seeded_kernel):
    """A bundle answering neither get_webapp nor get_gl_view (file.tools)
    is rolled back via cascade-delete after the renderability probe
    fails. The canvas ends up with no members."""
    cid = await _make_canvas_backend(seeded_kernel)
    r = await seeded_kernel.send(
        cid, {"type": "add_agent", "handler_module": "file.tools"}
    )
    assert "error" in r
    assert "neither get_webapp nor get_gl_view" in r["error"]
    canvas_agent = seeded_kernel.ctx.agents[cid]
    assert len(canvas_agent._children) == 0


async def test_add_agent_accepts_get_gl_view_only(seeded_kernel):
    """An agent answering only get_gl_view (not get_webapp) IS addable
    — GL-only agents like telemetry_pane belong on canvases too."""
    cid = await _make_canvas_backend(seeded_kernel)
    r = await seeded_kernel.send(
        cid, {"type": "add_agent", "handler_module": "telemetry_pane.tools"}
    )
    assert r.get("ok") is True, r
    assert r["member_id"] in r["members"]


async def test_remove_agent_cascades(seeded_kernel):
    cid = await _make_canvas_backend(seeded_kernel)
    r = await seeded_kernel.send(
        cid, {"type": "add_agent", "handler_module": "html_agent.tools"}
    )
    member_id = r["member_id"]
    r = await seeded_kernel.send(cid, {"type": "remove_agent", "agent_id": member_id})
    assert r == {"removed": True, "members": []}
    canvas_agent = seeded_kernel.ctx.agents[cid]
    assert member_id not in canvas_agent._children
    # Cascade actually killed the member's record + its presence in ctx.
    assert member_id not in seeded_kernel.ctx.agents


async def test_remove_agent_idempotent(seeded_kernel):
    cid = await _make_canvas_backend(seeded_kernel)
    events: list[dict] = []
    unsub = seeded_kernel.add_state_subscriber(events.append)
    try:
        r = await seeded_kernel.send(
            cid, {"type": "remove_agent", "agent_id": "not_a_member"}
        )
    finally:
        unsub()
    assert r == {"removed": False, "members": []}
    member_events = [
        e
        for e in events
        if e.get("kind") == "emit"
        and e.get("agent_id") == cid
        and e.get("payload", {}).get("type") == "members_updated"
    ]
    assert member_events == []


async def test_reflect_member_count_tracks_children(seeded_kernel):
    cid = await _make_canvas_backend(seeded_kernel)
    await seeded_kernel.send(
        cid, {"type": "add_agent", "handler_module": "html_agent.tools"}
    )
    await seeded_kernel.send(
        cid, {"type": "add_agent", "handler_module": "html_agent.tools"}
    )
    r = await seeded_kernel.send(cid, {"type": "reflect"})
    assert r["member_count"] == 2
    assert "members_updated" in r["emits"]
    assert "add_agent" in r["verbs"]
    assert "remove_agent" in r["verbs"]
    assert "list_members" in r["verbs"]
