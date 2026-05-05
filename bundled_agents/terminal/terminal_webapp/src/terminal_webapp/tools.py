"""terminal_webapp — UI agent fronting a terminal_backend.

Holds an `upstream_id` pointing at the backend it renders. No state, no
compute. Real work happens in `webapp/index.html` (read fresh on every
GET so dev edits show up without restarts).
"""

from __future__ import annotations

from importlib import resources


def _bundled_html() -> str:
    """Read the bundle's index.html from package resources.
    NOT cached — each request re-reads so iterative dev (editing the
    HTML and refreshing the browser) works without a kernel restart.
    """
    return (resources.files("terminal_webapp") / "webapp" / "index.html").read_text(
        "utf-8"
    )


# ─── verbs ──────────────────────────────────────────────────────


async def _reflect(id, payload, kernel):
    """Identity + upstream_id binding. No args."""
    rec = kernel.get(id) or {}
    return {
        "id": id,
        "sentence": "xterm UI fronting an upstream terminal backend.",
        "upstream_id": rec.get("upstream_id"),
        "verbs": {
            n: (f.__doc__ or "").strip().splitlines()[0] for n, f in VERBS.items()
        },
    }


async def _get_webapp(id, payload, kernel):
    """No args. Canvas-facing UI descriptor: {url, default_width, default_height, title}."""
    return {
        "url": f"/{id}/",
        "default_width": 600,
        "default_height": 400,
        "title": "xterm",
    }


async def _render_html(id, payload, kernel):
    """No args. Returns {html:str} — bundled webapp/index.html, read fresh on each call."""
    return {"html": _bundled_html()}


async def _boot(id, payload, kernel):
    """No-op. Returns None."""
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
        return {"error": f"terminal_webapp: unknown type {t!r}"}
    return await fn(id, payload, kernel)
