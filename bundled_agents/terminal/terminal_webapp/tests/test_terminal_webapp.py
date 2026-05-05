"""terminal_webapp — xterm UI agent."""

from __future__ import annotations


async def _make(kernel, upstream_id="some_backend"):
    rec = await kernel.send(
        "core",
        {
            "type": "create_agent",
            "handler_module": "terminal_webapp.tools",
            "upstream_id": upstream_id,
        },
    )
    return rec["id"]


async def test_reflect_returns_upstream_id(seeded_kernel):
    aid = await _make(seeded_kernel, upstream_id="terminal_backend_x")
    r = await seeded_kernel.send(aid, {"type": "reflect"})
    assert r["upstream_id"] == "terminal_backend_x"
    assert "get_webapp" in r["verbs"]


async def test_get_webapp_returns_descriptor(seeded_kernel):
    aid = await _make(seeded_kernel)
    r = await seeded_kernel.send(aid, {"type": "get_webapp"})
    assert r["url"] == f"/{aid}/"
    assert r["default_width"] == 600
    assert r["default_height"] == 400
    assert r["title"] == "xterm"


async def test_unknown_verb_errors(seeded_kernel):
    aid = await _make(seeded_kernel)
    r = await seeded_kernel.send(aid, {"type": "garbage"})
    assert "error" in r
