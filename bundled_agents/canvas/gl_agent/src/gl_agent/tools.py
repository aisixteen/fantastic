"""gl_agent — GL-view-as-a-record.

Mirror of html_agent for WebGL content. The agent's `gl_source` field
IS the GL-view JS body; the canvas host (canvas_webapp) compiles it
via `new Function('THREE','scene','t','onFrame','cleanup', source)`
and runs it. Each view gets its own `THREE.Group` container (injected
as `scene`) — the scene-graph analogue of an html_agent's iframe.

Spawn (WS):
    ws://host/core/ws  send:
      {"type":"call","target":"core","payload":{
          "type":"create_agent",
          "handler_module":"gl_agent.tools",
          "gl_source":"...JS body...",
          "title":"AVS",
          "display_name":"AVS bg"},"id":"1"}

Add to a canvas (WS):
    ws://host/<canvas_backend_id>/ws  send:
      {"type":"call","target":"<canvas_backend_id>","payload":{
          "type":"add_agent","agent_id":"<gl_agent_id>"},"id":"1"}

Edit live (WS):
    ws://host/<id>/ws  send:
      {"type":"call","target":"<id>","payload":{
          "type":"set_gl_source","source":"..."},"id":"1"}
    # set_gl_source emits `gl_source_changed` on this agent's inbox.
    # A canvas hosting the view watches the member and reinstalls it
    # in place — disposes the view's THREE.Group container, recompiles
    # the new source into a fresh one. Same agent id, no canvas
    # refresh. The GL analogue of html_agent's set_html → reload_html.

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
        "emits": {
            "gl_source_changed": "{type:'gl_source_changed', id} — set_gl_source fires this; a canvas hosting the view reinstalls it in place",
        },
    }


async def _get_gl_source(id, payload, kernel):
    """No args. Returns {source:str} — the raw JS body stored on the record."""
    rec = kernel.get(id) or {}
    return {"source": rec.get("gl_source", "")}


async def _set_gl_source(id, payload, kernel):
    """args: source:str (req), title:str?. Replaces gl_source (and optionally title) on the agent record, then emits `gl_source_changed` so a canvas hosting the view reinstalls it in place (dispose the view's container + recompile) — no remove/re-add, no canvas refresh."""
    src = payload.get("source")
    if not isinstance(src, str):
        return {"error": "gl_agent: source (str) required"}
    meta: dict = {"gl_source": src}
    if isinstance(payload.get("title"), str):
        meta["title"] = payload["title"]
    rec = kernel.update(id, **meta)
    if rec is None:
        return {"error": f"no agent {id!r}"}
    # Carry `id` in the payload: a canvas watches MANY GL members
    # (unlike an html iframe, which watches only itself), so the
    # consumer needs to know which view changed.
    await kernel.emit(id, {"type": "gl_source_changed", "id": id})
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
