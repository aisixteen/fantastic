"""canvas_backend — spatial discovery + explicit membership."""

from __future__ import annotations

from unittest.mock import patch

from canvas_backend.tools import _intersects, _rect


async def _make_canvas_backend(kernel):
    rec = await kernel.send(
        "core",
        {"type": "create_agent", "handler_module": "canvas_backend.tools"},
    )
    return rec["id"]


async def _make_html_agent(kernel, **meta):
    """A get_webapp-answering agent suitable for canvas membership."""
    rec = await kernel.send(
        "core",
        {"type": "create_agent", "handler_module": "html_agent.tools", **meta},
    )
    return rec["id"]


def test_intersects_overlapping():
    assert _intersects((0, 0, 10, 10), (5, 5, 10, 10))


def test_intersects_disjoint():
    assert not _intersects((0, 0, 10, 10), (100, 100, 10, 10))


def test_intersects_edge_touch_disjoint():
    """Boxes touching only at the edge should NOT intersect (strict)."""
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


async def test_discover_returns_intersecting_agents(seeded_kernel):
    cid = await _make_canvas_backend(seeded_kernel)
    # create an agent at coords
    a = await seeded_kernel.send(
        "core",
        {
            "type": "create_agent",
            "handler_module": "file.tools",
            "x": 100,
            "y": 100,
            "width": 50,
            "height": 50,
        },
    )
    # Discover query overlapping the agent
    r = await seeded_kernel.send(
        cid, {"type": "discover", "x": 0, "y": 0, "w": 200, "h": 200}
    )
    ids = {x["id"] for x in r["agents"]}
    assert a["id"] in ids


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


# ─── explicit membership ────────────────────────────────────────


async def test_list_members_empty_by_default(seeded_kernel):
    cid = await _make_canvas_backend(seeded_kernel)
    r = await seeded_kernel.send(cid, {"type": "list_members"})
    assert r == {"members": []}


async def test_add_agent_appends(seeded_kernel):
    cid = await _make_canvas_backend(seeded_kernel)
    hid = await _make_html_agent(seeded_kernel)
    r = await seeded_kernel.send(cid, {"type": "add_agent", "agent_id": hid})
    assert r == {"ok": True, "members": [hid]}
    # Persisted on the canvas record.
    assert seeded_kernel.get(cid)["members"] == [hid]


async def test_add_agent_emits_members_updated(seeded_kernel):
    cid = await _make_canvas_backend(seeded_kernel)
    hid = await _make_html_agent(seeded_kernel)
    captured: list[tuple[str, dict]] = []
    real_emit = seeded_kernel.emit

    async def spy_emit(target, payload):
        captured.append((target, dict(payload)))
        return await real_emit(target, payload)

    with patch.object(seeded_kernel, "emit", spy_emit):
        await seeded_kernel.send(cid, {"type": "add_agent", "agent_id": hid})
    own = [p for t, p in captured if t == cid]
    member_events = [p for p in own if p.get("type") == "members_updated"]
    assert len(member_events) == 1
    assert member_events[0]["members"] == [hid]


async def test_add_agent_idempotent(seeded_kernel):
    cid = await _make_canvas_backend(seeded_kernel)
    hid = await _make_html_agent(seeded_kernel)
    await seeded_kernel.send(cid, {"type": "add_agent", "agent_id": hid})
    # Second add — already present, no-op.
    captured: list[tuple[str, dict]] = []
    real_emit = seeded_kernel.emit

    async def spy_emit(target, payload):
        captured.append((target, dict(payload)))
        return await real_emit(target, payload)

    with patch.object(seeded_kernel, "emit", spy_emit):
        r2 = await seeded_kernel.send(cid, {"type": "add_agent", "agent_id": hid})
    assert r2 == {"ok": True, "members": [hid], "already": True}
    # No second members_updated event.
    member_events = [
        p for t, p in captured if t == cid and p.get("type") == "members_updated"
    ]
    assert member_events == []


async def test_add_agent_refuses_unknown_id(seeded_kernel):
    cid = await _make_canvas_backend(seeded_kernel)
    r = await seeded_kernel.send(cid, {"type": "add_agent", "agent_id": "bogus_xxx"})
    assert "error" in r and "no agent" in r["error"]


async def test_add_agent_refuses_non_renderable_target(seeded_kernel, file_agent):
    """file agent answers neither get_webapp nor get_gl_view → cannot
    be added to a canvas (nothing to render)."""
    cid = await _make_canvas_backend(seeded_kernel)
    r = await seeded_kernel.send(cid, {"type": "add_agent", "agent_id": file_agent})
    assert "error" in r
    assert "neither get_webapp nor get_gl_view" in r["error"]
    assert seeded_kernel.get(cid).get("members") in (None, [])


async def test_add_agent_accepts_get_gl_view_only(seeded_kernel):
    """An agent answering only get_gl_view (not get_webapp) IS addable
    — GL-only agents like telemetry_pane belong on canvases too."""
    cid = await _make_canvas_backend(seeded_kernel)
    rec = await seeded_kernel.send(
        "core",
        {"type": "create_agent", "handler_module": "telemetry_pane.tools"},
    )
    tid = rec["id"]
    r = await seeded_kernel.send(cid, {"type": "add_agent", "agent_id": tid})
    assert r.get("ok") is True, r
    assert tid in r["members"]


async def test_add_agent_requires_string_id(seeded_kernel):
    cid = await _make_canvas_backend(seeded_kernel)
    r = await seeded_kernel.send(cid, {"type": "add_agent"})
    assert "error" in r
    r = await seeded_kernel.send(cid, {"type": "add_agent", "agent_id": 42})
    assert "error" in r


async def test_remove_agent_drops_id(seeded_kernel):
    cid = await _make_canvas_backend(seeded_kernel)
    hid = await _make_html_agent(seeded_kernel)
    await seeded_kernel.send(cid, {"type": "add_agent", "agent_id": hid})
    r = await seeded_kernel.send(cid, {"type": "remove_agent", "agent_id": hid})
    assert r == {"removed": True, "members": []}
    assert seeded_kernel.get(cid)["members"] == []


async def test_remove_agent_idempotent(seeded_kernel):
    cid = await _make_canvas_backend(seeded_kernel)
    captured: list[tuple[str, dict]] = []
    real_emit = seeded_kernel.emit

    async def spy_emit(target, payload):
        captured.append((target, dict(payload)))
        return await real_emit(target, payload)

    with patch.object(seeded_kernel, "emit", spy_emit):
        r = await seeded_kernel.send(
            cid, {"type": "remove_agent", "agent_id": "not_a_member"}
        )
    assert r == {"removed": False, "members": []}
    # No members_updated emitted for a no-op remove.
    member_events = [
        p for t, p in captured if t == cid and p.get("type") == "members_updated"
    ]
    assert member_events == []


async def test_reflect_member_count_tracks_list(seeded_kernel):
    cid = await _make_canvas_backend(seeded_kernel)
    h1 = await _make_html_agent(seeded_kernel)
    h2 = await _make_html_agent(seeded_kernel)
    await seeded_kernel.send(cid, {"type": "add_agent", "agent_id": h1})
    await seeded_kernel.send(cid, {"type": "add_agent", "agent_id": h2})
    r = await seeded_kernel.send(cid, {"type": "reflect"})
    assert r["member_count"] == 2
    assert "members_updated" in r["emits"]
    assert "add_agent" in r["verbs"]
    assert "remove_agent" in r["verbs"]
    assert "list_members" in r["verbs"]
