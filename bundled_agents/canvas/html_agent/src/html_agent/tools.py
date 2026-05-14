"""html_agent — UI-as-a-record.

The agent's body lives at `<agent_dir>/index.html` — a plain file
next to `agent.json`. `agent.json` stays lean (identity + display
fields); the HTML is editable in a text editor / shell without JSON
escaping. Webapp serves it at `/<id>/` with transport.js injected.

Spawn (WS — body via `html` field on the create payload):
    {"type":"call","target":"core","payload":{
        "type":"create_agent",
        "handler_module":"html_agent.tools",
        "html":"<h1>hi</h1>",
        "display_name":"Panel"},"id":"1"}

Edit live (WS):
    {"type":"call","target":"<id>","payload":{
        "type":"set_html","html":"…"},"id":"1"}   # emits reload_html

Legacy compat: if a record on disk still carries inline `html_content`
(pre-rewrite), `_boot` migrates it out to `index.html` and strips
the field on first run. Idempotent.
"""

from __future__ import annotations

from importlib import resources
from pathlib import Path


# ─── file storage ───────────────────────────────────────────────


def _html_path(agent) -> Path:
    return agent._root_path / "index.html"


def _read_html(agent) -> str | None:
    p = _html_path(agent)
    if p.exists():
        return p.read_text(encoding="utf-8")
    return None


def _write_html(agent, html: str) -> int:
    p = _html_path(agent)
    p.write_text(html, encoding="utf-8")
    return len(html.encode("utf-8"))


# ─── verbs ──────────────────────────────────────────────────────


async def _reflect(id, payload, agent):
    """Identity + html byte size + display name. No args."""
    rec = agent.get(id) or {}
    p = _html_path(agent)
    html_bytes = p.stat().st_size if p.exists() else 0
    return {
        "id": id,
        "sentence": "UI-as-record. Body stored at <agent_dir>/index.html; served at /<id>/.",
        "display_name": rec.get("display_name", id),
        "html_bytes": html_bytes,
        "html_path": str(p),
        "verbs": {
            n: (f.__doc__ or "").strip().splitlines()[0] for n, f in VERBS.items()
        },
        "emits": {
            "reload_html": "{type:'reload_html'} — universal page-reload signal; transport.js subscribes on every served page",
        },
    }


async def _get_html(id, payload, agent):
    """No args. Returns {html:str} — current body, or empty string if unset (transport NOT injected; that's webapp's job at /<id>/)."""
    return {"html": _read_html(agent) or ""}


async def _set_html(id, payload, agent):
    """args: html:str (req). Writes `<agent_dir>/index.html`; emits reload_html so open browser tabs refresh."""
    html = payload.get("html", "")
    if not isinstance(html, str):
        return {"error": "html_agent: html must be a string"}
    bytes_written = _write_html(agent, html)
    await agent.emit(id, {"type": "reload_html"})
    return {"ok": True, "bytes": bytes_written}


async def _render_html(id, payload, agent):
    """No args. Returns {html:str} — body served at /<id>/. Empty
    record → placeholder template with a WS set_html instruction."""
    html = _read_html(agent)
    if not html:
        tpl = (
            resources.files("html_agent") / "templates" / "placeholder.html"
        ).read_text("utf-8")
        html = tpl.replace("{{agent_id}}", id)
    return {"html": html}


async def _get_webapp(id, payload, agent):
    """No args. Returns canvas-facing UI descriptor: {url, default_width, default_height, title}."""
    rec = agent.get(id) or {}
    return {
        "url": f"/{id}/",
        "default_width": int(rec.get("width") or 480),
        "default_height": int(rec.get("height") or 360),
        "title": rec.get("display_name") or "html",
    }


async def _boot(id, payload, agent):
    """Idempotent. If `create_agent` was called with an `html` (or
    legacy `html_content`) field in the payload, the substrate stored
    it on the agent's `_meta` dict. Migrate the body out to
    `<agent_dir>/index.html` and strip the field. Re-persists agent.json
    after — no `html`/`html_content` should ever appear in a record."""
    migrated = False
    for key in ("html", "html_content"):
        if key in agent._meta:
            html = agent._meta.pop(key)
            if isinstance(html, str) and html:
                _write_html(agent, html)
            migrated = True
    if migrated:
        agent._persist()
    return None


# ─── dispatch ───────────────────────────────────────────────────


VERBS = {
    "reflect": _reflect,
    "get_html": _get_html,
    "set_html": _set_html,
    "render_html": _render_html,
    "get_webapp": _get_webapp,
    "boot": _boot,
}


async def handler(id: str, payload: dict, agent) -> dict | None:
    t = payload.get("type")
    fn = VERBS.get(t)
    if fn is None:
        return {"error": f"html_agent: unknown type {t!r}"}
    return await fn(id, payload, agent)
