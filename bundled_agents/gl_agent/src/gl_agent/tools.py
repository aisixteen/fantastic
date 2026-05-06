"""gl_agent — GL-view-as-a-record.

Mirror of html_agent for WebGL content. The agent's `gl_source` field
IS the GL-view JS body; the canvas host (canvas_webapp) compiles it
via `new Function('THREE','scene','t','onFrame','cleanup', source)`
and runs it inside its WebGL scene.

Spawn:
    POST /core/call
      {"type":"create_agent",
       "handler_module":"gl_agent.tools",
       "gl_source":"...JS body...",
       "title":"AVS",
       "display_name":"AVS bg"}

Add to a canvas:
    POST /<canvas_backend_id>/call
      {"type":"add_agent","agent_id":"<gl_agent_id>"}

Edit live:
    POST /<id>/call {"type":"set_gl_source","source":"..."}
    # The canvas does not auto-reinstall a changed source — operator
    # removes + re-adds the agent on the canvas to pick up the new
    # body, OR refreshes the tab. Mirrors how html_agent's
    # `set_html` relies on transport.js's reload_html listener;
    # there's no equivalent universal subscription on the GL side
    # because GL views run inside the canvas's scene, not as
    # iframes.

This is the GL parallel to html_agent: a generic "carry source on
the record, answer the dispatch verb" container so per-project
visualizations don't have to grow into full bundles.
"""

from __future__ import annotations


# ─── verbs ──────────────────────────────────────────────────────


async def _reflect(id, payload, kernel):
    """Identity + source size + title. No args."""
    rec = kernel.get(id) or {}
    src = rec.get("gl_source") or ""
    return {
        "id": id,
        "sentence": "GL-view-as-record. gl_source stored on agent.json; rendered by canvas hosts that probe get_gl_view.",
        "display_name": rec.get("display_name", id),
        "title": rec.get("title", ""),
        "source_bytes": len(src.encode("utf-8")),
        "verbs": {
            n: (f.__doc__ or "").strip().splitlines()[0] for n, f in VERBS.items()
        },
    }


async def _get_gl_source(id, payload, kernel):
    """No args. Returns {source:str} — the raw JS body stored on the record."""
    rec = kernel.get(id) or {}
    return {"source": rec.get("gl_source", "")}


async def _set_gl_source(id, payload, kernel):
    """args: source:str (req), title:str?. Replaces gl_source (and optionally title) on the agent record. Canvases do NOT auto-reinstall the view on change — remove + re-add the agent on the canvas to pick up the new body."""
    src = payload.get("source")
    if not isinstance(src, str):
        return {"error": "gl_agent: source (str) required"}
    meta: dict = {"gl_source": src}
    if isinstance(payload.get("title"), str):
        meta["title"] = payload["title"]
    rec = kernel.update(id, **meta)
    if rec is None:
        return {"error": f"no agent {id!r}"}
    return {"ok": True, "bytes": len(src.encode("utf-8"))}


async def _get_gl_view(id, payload, kernel):
    """No args. Returns {source:str, title:str} — what the canvas host's GL probe consumes. Source comes from agent.gl_source; title falls back to display_name then id when unset."""
    rec = kernel.get(id) or {}
    return {
        "source": rec.get("gl_source", ""),
        "title": rec.get("title") or rec.get("display_name") or id,
    }


async def _boot(id, payload, kernel):
    """No-op. gl_agent is a passive record; its source runs only when a canvas host installs it."""
    return None


# ─── dispatch ───────────────────────────────────────────────────


VERBS = {
    "reflect": _reflect,
    "get_gl_source": _get_gl_source,
    "set_gl_source": _set_gl_source,
    "get_gl_view": _get_gl_view,
    "boot": _boot,
}


async def handler(id: str, payload: dict, kernel) -> dict | None:
    t = payload.get("type")
    fn = VERBS.get(t)
    if fn is None:
        return {"error": f"gl_agent: unknown type {t!r}"}
    return await fn(id, payload, kernel)
