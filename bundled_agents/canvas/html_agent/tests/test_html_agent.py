"""html_agent — UI-as-record verb behavior."""

from __future__ import annotations

from pathlib import Path


async def _make_html(kernel, **meta):
    rec = await kernel.send(
        "core",
        {"type": "create_agent", "handler_module": "html_agent.tools", **meta},
    )
    return rec["id"]


async def test_reflect_no_content(seeded_kernel):
    aid = await _make_html(seeded_kernel)
    r = await seeded_kernel.send(aid, {"type": "reflect"})
    assert r["id"] == aid
    assert r["html_bytes"] == 0
    assert r["display_name"] == aid  # default to id when unset
    assert "set_html" in r["verbs"]
    # html_path is the file backing the body, next to agent.json.
    assert r["html_path"].endswith(f"{aid}/index.html")
    # Universal page-reload event; transport.js subscribes on every served page.
    assert "reload_html" in r["emits"]


async def test_reflect_with_content_and_display_name(seeded_kernel):
    aid = await _make_html(seeded_kernel, html="<h1>hi</h1>", display_name="Panel")
    r = await seeded_kernel.send(aid, {"type": "reflect"})
    assert r["display_name"] == "Panel"
    assert r["html_bytes"] == len("<h1>hi</h1>".encode("utf-8"))


async def test_set_html_persists_and_emits(seeded_kernel):
    aid = await _make_html(seeded_kernel)
    events: list[dict] = []
    unsub = seeded_kernel.add_state_subscriber(events.append)
    try:
        r = await seeded_kernel.send(aid, {"type": "set_html", "html": "<p>v2</p>"})
    finally:
        unsub()
    assert r["ok"] is True
    assert r["bytes"] == len("<p>v2</p>".encode("utf-8"))
    # File-backed body — agent.json stays lean; the html lives next door.
    html_path = Path(seeded_kernel.ctx.agents[aid]._root_path) / "index.html"
    assert html_path.read_text() == "<p>v2</p>"
    assert "html_content" not in seeded_kernel.get(aid)
    # Emitted reload_html on the agent's own inbox so transport.js's
    # universal listener triggers location.reload() in any open tab.
    emits_on_self = [
        e for e in events if e.get("kind") == "emit" and e.get("agent_id") == aid
    ]
    assert any(e.get("payload", {}).get("type") == "reload_html" for e in emits_on_self)


async def test_set_html_rejects_non_string(seeded_kernel):
    aid = await _make_html(seeded_kernel)
    r = await seeded_kernel.send(aid, {"type": "set_html", "html": 42})
    assert "error" in r and "string" in r["error"]


async def test_get_html_returns_stored(seeded_kernel):
    aid = await _make_html(seeded_kernel, html="<b>stored</b>")
    r = await seeded_kernel.send(aid, {"type": "get_html"})
    assert r["html"] == "<b>stored</b>"


async def test_render_html_returns_stored_content(seeded_kernel):
    """No prepended scripts, no wrapping — the served HTML is exactly
    the stored body. Reload-on-update is handled universally by
    transport.js (which the webapp injects), NOT by this bundle."""
    aid = await _make_html(seeded_kernel, html="<h1>hi</h1>")
    r = await seeded_kernel.send(aid, {"type": "render_html"})
    assert r["html"] == "<h1>hi</h1>"
    # Drift guard: no inline reload listener leaked into the body.
    assert "location.reload" not in r["html"]
    assert "reload_html" not in r["html"]


async def test_render_html_placeholder_when_unset(seeded_kernel):
    aid = await _make_html(seeded_kernel)
    r = await seeded_kernel.send(aid, {"type": "render_html"})
    # Placeholder mentions the agent id and the set_html verb so a
    # human/agent landing on /<id>/ knows what to do next.
    assert aid in r["html"]
    assert "set_html" in r["html"]


async def test_boot_migrates_legacy_html_content(seeded_kernel):
    """Legacy record with inline `html_content` migrates to a file on
    first boot. Field is stripped from agent.json; body lives in
    `<agent_dir>/index.html`. Idempotent."""
    aid = await _make_html(seeded_kernel, html_content="<legacy>old</legacy>")
    # _boot fired automatically on create — migration already done.
    rec = seeded_kernel.get(aid)
    assert "html_content" not in rec
    html_path = Path(seeded_kernel.ctx.agents[aid]._root_path) / "index.html"
    assert html_path.read_text() == "<legacy>old</legacy>"
    # render_html serves from the file.
    r = await seeded_kernel.send(aid, {"type": "render_html"})
    assert r["html"] == "<legacy>old</legacy>"


async def test_get_webapp_default_size_and_title(seeded_kernel):
    aid = await _make_html(seeded_kernel)
    r = await seeded_kernel.send(aid, {"type": "get_webapp"})
    assert r["url"] == f"/{aid}/"
    assert r["default_width"] == 480
    assert r["default_height"] == 360
    assert r["title"] == "html"


async def test_get_webapp_uses_record_dims_and_display_name(seeded_kernel):
    aid = await _make_html(
        seeded_kernel, width=600, height=400, display_name="Control Panel"
    )
    r = await seeded_kernel.send(aid, {"type": "get_webapp"})
    assert r["default_width"] == 600
    assert r["default_height"] == 400
    assert r["title"] == "Control Panel"


async def test_unknown_verb_errors(seeded_kernel):
    aid = await _make_html(seeded_kernel)
    r = await seeded_kernel.send(aid, {"type": "garbage"})
    assert "error" in r
