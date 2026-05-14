"""canvas_webapp — spatial UI agent fronting a canvas_backend.

Holds an `upstream_id` pointing at the canvas backend whose
`list_members` verb scopes which agents the UI surfaces. Two
presentation layers per agent: DOM (existing iframe) for agents
answering `get_webapp`, and GL (Three.js scene) for agents answering
`get_gl_view`. An agent answering both gets both — telemetry
overlays on top of an iframe is a first-class case.

Real work in `webapp/index.html`.
"""

from __future__ import annotations

from importlib import resources


def _bundled_html() -> str:
    """Read the bundle's index.html from package resources, NOT cached."""
    return (resources.files("canvas_webapp") / "webapp" / "index.html").read_text(
        "utf-8"
    )


# ─── verbs ──────────────────────────────────────────────────────


async def _reflect(id, payload, kernel):
    """Identity + upstream + file_agent_id binding. No args."""
    rec = kernel.get(id) or {}
    return {
        "id": id,
        "sentence": "Spatial canvas UI fronting an upstream canvas backend.",
        "upstream_id": rec.get("upstream_id"),
        "file_agent_id": rec.get("file_agent_id"),
        "verbs": {
            n: (f.__doc__ or "").strip().splitlines()[0] for n, f in VERBS.items()
        },
    }


async def _get_webapp(id, payload, kernel):
    """No args. Returns canvas UI descriptor: {url, default_width, default_height, title}."""
    return {
        "url": f"/{id}/",
        "default_width": 800,
        "default_height": 600,
        "title": "canvas",
    }


async def _render_html(id, payload, kernel):
    """No args. Returns {html:str} — bundled webapp/index.html, read fresh on each call."""
    return {"html": _bundled_html()}


async def _boot(id, payload, agent):
    """Idempotent first-boot wiring: ensure a canvas_backend child
    exists. Subsequent boots find the existing child from disk."""
    BACKEND_HM = "canvas_backend.tools"
    has_backend = any(c.handler_module == BACKEND_HM for c in agent._children.values())
    if has_backend:
        return None
    rec = agent.create(BACKEND_HM)
    if "error" in rec:
        return rec
    agent.update(id, upstream_id=rec["id"])
    await agent.send(rec["id"], {"type": "boot"})
    return None


# ─── dispatch ───────────────────────────────────────────────────


VERBS = {
    "reflect": _reflect,
    "get_webapp": _get_webapp,
    "render_html": _render_html,
    "boot": _boot,
}


async def handler(id: str, payload: dict, kernel) -> dict | None:
    t = payload.get("type")
    fn = VERBS.get(t)
    if fn is None:
        return {"error": f"canvas_webapp: unknown type {t!r}"}
    return await fn(id, payload, kernel)
