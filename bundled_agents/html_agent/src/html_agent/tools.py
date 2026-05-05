"""html_agent — UI-as-a-record.

The agent's `html_content` field IS the page; webapp serves it at
`/<id>/` (transport.js auto-injected). Inside the served page,
`fantastic_transport()` lets the JS call any verb on any agent.

Spawn:
    POST /core/call
      {"type":"create_agent",
       "handler_module":"html_agent.tools",
       "html_content":"<h1>hi</h1>",
       "display_name":"Panel"}

Edit live:
    POST /<id>/call {"type":"set_html","html":"…"}   # emits reload_html

`reload_html` is the universal "this agent wants its open pages to
reload" event — transport.js subscribes to it on every served page,
so set_html (or anyone calling `t.emit(<id>, {type:'reload_html'})`)
closes the loop without any per-bundle script injection.

The webapp duck-types `render_html` — any agent that returns
`{html:str}` from that verb gets its page served (with transport
injected). html_agent is just the canonical implementer.
"""

from __future__ import annotations


# ─── verbs ──────────────────────────────────────────────────────


async def _reflect(id, payload, kernel):
    """Identity + html size + display name. No args."""
    rec = kernel.get(id) or {}
    html = rec.get("html_content") or ""
    return {
        "id": id,
        "sentence": "UI-as-record. html_content stored on agent.json; served at /<id>/.",
        "display_name": rec.get("display_name", id),
        "html_bytes": len(html.encode("utf-8")),
        "verbs": {
            n: (f.__doc__ or "").strip().splitlines()[0] for n, f in VERBS.items()
        },
        "emits": {
            "reload_html": "{type:'reload_html'} — universal page-reload signal; transport.js subscribes on every served page",
        },
    }


async def _get_html(id, payload, kernel):
    """No args. Returns {html:str} — the current page (transport NOT injected; that's webapp's job at /<id>/)."""
    rec = kernel.get(id) or {}
    return {"html": rec.get("html_content", "")}


async def _set_html(id, payload, kernel):
    """args: html:str (req). Replaces html_content on the agent record; emits reload_html so open browser tabs refresh."""
    html = payload.get("html", "")
    if not isinstance(html, str):
        return {"error": "html_agent: html must be a string"}
    rec = kernel.update(id, html_content=html)
    if rec is None:
        return {"error": f"no agent {id!r}"}
    await kernel.emit(id, {"type": "reload_html"})
    return {"ok": True, "bytes": len(html.encode("utf-8"))}


async def _render_html(id, payload, kernel):
    """No args. Returns {html:str} — what the webapp serves at /<id>/. The reload-on-update loop is closed by transport.js's universal `reload_html` listener; nothing to inject here."""
    rec = kernel.get(id) or {}
    html = rec.get("html_content")
    if not html:
        # Editable hint so a freshly created html_agent doesn't 500.
        html = (
            "<!doctype html>"
            "<body style='font:13px/1.5 system-ui;padding:24px;color:#aac;background:#0a0a14'>"
            f"<h2 style='color:#fff'>{id}</h2>"
            "<p>This <code>html_agent</code> has no <code>html_content</code> yet.</p>"
            "<pre style='background:#12121f;padding:12px;border-radius:6px;color:#cde'>"
            f'POST /{id}/call\n  {{"type":"set_html","html":"&lt;your html&gt;"}}'
            "</pre>"
            "</body>"
        )
    return {"html": html}


async def _get_webapp(id, payload, kernel):
    """No args. Returns canvas-facing UI descriptor: {url, default_width, default_height, title}."""
    rec = kernel.get(id) or {}
    return {
        "url": f"/{id}/",
        "default_width": int(rec.get("width") or 480),
        "default_height": int(rec.get("height") or 360),
        "title": rec.get("display_name") or "html",
    }


async def _boot(id, payload, kernel):
    """No-op. html_agent is fully browser-driven."""
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


async def handler(id: str, payload: dict, kernel) -> dict | None:
    t = payload.get("type")
    fn = VERBS.get(t)
    if fn is None:
        return {"error": f"html_agent: unknown type {t!r}"}
    return await fn(id, payload, kernel)
