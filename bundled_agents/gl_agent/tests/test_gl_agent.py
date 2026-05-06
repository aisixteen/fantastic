"""gl_agent bundle tests.

Mirror of html_agent's test surface. Drift guards on:
- Verb surface (reflect lists set_gl_source / get_gl_view)
- get_gl_view returns the gl_source field from the record
- set_gl_source updates the record + survives reload
- title falls back to display_name → id
"""

from __future__ import annotations


async def _make(kernel, **meta):
    rec = await kernel.send(
        "core",
        {"type": "create_agent", "handler_module": "gl_agent.tools", **meta},
    )
    return rec["id"]


async def test_reflect_lists_verbs(seeded_kernel):
    aid = await _make(seeded_kernel)
    r = await seeded_kernel.send(aid, {"type": "reflect"})
    for v in ("get_gl_source", "set_gl_source", "get_gl_view", "boot", "reflect"):
        assert v in r["verbs"], f"missing verb {v}"


async def test_get_gl_view_reads_record_source(seeded_kernel):
    src = "// hello"
    aid = await _make(seeded_kernel, gl_source=src, title="hi")
    r = await seeded_kernel.send(aid, {"type": "get_gl_view"})
    assert r == {"source": src, "title": "hi"}


async def test_get_gl_view_title_fallback(seeded_kernel):
    """No `title` set → fall back to display_name → id."""
    aid = await _make(seeded_kernel, gl_source="x", display_name="MY-VIS")
    r = await seeded_kernel.send(aid, {"type": "get_gl_view"})
    assert r["title"] == "MY-VIS"

    aid2 = await _make(seeded_kernel, gl_source="x")
    r2 = await seeded_kernel.send(aid2, {"type": "get_gl_view"})
    assert r2["title"] == aid2  # id fallback


async def test_set_gl_source_updates_record(seeded_kernel):
    aid = await _make(seeded_kernel, gl_source="old")
    r = await seeded_kernel.send(aid, {"type": "set_gl_source", "source": "new"})
    assert r["ok"] is True
    after = await seeded_kernel.send(aid, {"type": "get_gl_view"})
    assert after["source"] == "new"


async def test_set_gl_source_can_update_title(seeded_kernel):
    aid = await _make(seeded_kernel, gl_source="x", title="A")
    await seeded_kernel.send(
        aid, {"type": "set_gl_source", "source": "y", "title": "B"}
    )
    after = await seeded_kernel.send(aid, {"type": "get_gl_view"})
    assert after == {"source": "y", "title": "B"}


async def test_set_gl_source_requires_string(seeded_kernel):
    aid = await _make(seeded_kernel)
    r = await seeded_kernel.send(aid, {"type": "set_gl_source", "source": 42})
    assert "error" in r


async def test_get_gl_source_returns_raw(seeded_kernel):
    """get_gl_source mirrors get_html — the raw stored body, no canvas envelope."""
    aid = await _make(seeded_kernel, gl_source="raw-js")
    r = await seeded_kernel.send(aid, {"type": "get_gl_source"})
    assert r == {"source": "raw-js"}


async def test_unknown_verb_errors(seeded_kernel):
    aid = await _make(seeded_kernel)
    r = await seeded_kernel.send(aid, {"type": "garbage"})
    assert "error" in r


async def test_canvas_backend_accepts_gl_agent(seeded_kernel):
    """End-to-end: a gl_agent answering get_gl_view must satisfy
    canvas_backend.add_agent's dual-verb gate just like a bundled GL
    view (telemetry_pane). This is the whole point of the
    abstraction — drop-in GL views without scaffolding a bundle."""
    cb = (
        await seeded_kernel.send(
            "core",
            {"type": "create_agent", "handler_module": "canvas_backend.tools"},
        )
    )["id"]
    gl = await _make(seeded_kernel, gl_source="// scene.background = null;")
    r = await seeded_kernel.send(cb, {"type": "add_agent", "agent_id": gl})
    assert r.get("ok") is True
    assert gl in r["members"]
