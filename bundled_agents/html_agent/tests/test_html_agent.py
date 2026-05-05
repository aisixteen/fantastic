"""html_agent — UI-as-record verb behavior."""

from __future__ import annotations

from unittest.mock import patch


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
    # Universal page-reload event; transport.js subscribes on every served page.
    assert "reload_html" in r["emits"]
    assert "html_updated" not in r["emits"]  # renamed; no BC alias


async def test_reflect_with_content_and_display_name(seeded_kernel):
    aid = await _make_html(
        seeded_kernel, html_content="<h1>hi</h1>", display_name="Panel"
    )
    r = await seeded_kernel.send(aid, {"type": "reflect"})
    assert r["display_name"] == "Panel"
    assert r["html_bytes"] == len("<h1>hi</h1>".encode("utf-8"))


async def test_set_html_persists_and_emits(seeded_kernel):
    aid = await _make_html(seeded_kernel)
    captured: list[tuple[str, dict]] = []
    real_emit = seeded_kernel.emit

    async def spy_emit(target, payload):
        captured.append((target, dict(payload)))
        return await real_emit(target, payload)

    with patch.object(seeded_kernel, "emit", spy_emit):
        r = await seeded_kernel.send(aid, {"type": "set_html", "html": "<p>v2</p>"})
    assert r["ok"] is True
    assert r["bytes"] == len("<p>v2</p>".encode("utf-8"))
    assert seeded_kernel.get(aid)["html_content"] == "<p>v2</p>"
    # Emitted reload_html on the agent's own inbox so transport.js's
    # universal listener triggers location.reload() in any open tab.
    own = [p for t, p in captured if t == aid]
    assert any(p.get("type") == "reload_html" for p in own)


async def test_set_html_rejects_non_string(seeded_kernel):
    aid = await _make_html(seeded_kernel)
    r = await seeded_kernel.send(aid, {"type": "set_html", "html": 42})
    assert "error" in r and "string" in r["error"]


async def test_get_html_returns_stored(seeded_kernel):
    aid = await _make_html(seeded_kernel, html_content="<b>stored</b>")
    r = await seeded_kernel.send(aid, {"type": "get_html"})
    assert r["html"] == "<b>stored</b>"


async def test_render_html_returns_stored_content(seeded_kernel):
    """No prepended scripts, no wrapping — the served HTML is exactly
    the stored html_content. Reload-on-update is handled universally
    by transport.js (which the webapp injects), NOT by this bundle."""
    aid = await _make_html(seeded_kernel, html_content="<h1>hi</h1>")
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
