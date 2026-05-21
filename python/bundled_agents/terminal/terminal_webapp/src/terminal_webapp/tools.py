"""terminal_webapp — UI agent fronting a terminal_backend.

Owns its terminal_backend as a **child agent**. Created idempotently on
first boot via `agent.create("terminal_backend.tools")`; the substrate
persists the child record under terminal_webapp's directory. On every
subsequent boot the child already exists from disk — `_boot` finds it
and skips creation. Cascade delete via the substrate's `delete_agent`
removes terminal_webapp + cascades through terminal_backend, whose
`on_delete` hook kills the PTY before the records are removed from disk.

`upstream_id` field on this agent's record tracks the bound backend's
id (for frontends discovering the pair). Lifecycle is governed by the
parent-child relationship, not by the field.
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
    """No args. Canvas-facing UI descriptor: {url, default_width, default_height, title, header_buttons}.

    `header_buttons` is the duck-typed contract for adding chips to
    the canvas frame's header (next to lock/reload/close). Each entry:
      id     — string, identifies the button across postMessage round-trips
      glyph  — single-char string rendered as the chip's label
      title  — tooltip on hover
      toggle — bool; if true, the canvas tracks active/inactive state
               based on `header_button_state` echoes from the iframe
    """
    return {
        "url": f"/{id}/",
        "default_width": 600,
        "default_height": 400,
        "title": "xterm",
        "header_buttons": [
            {
                "id": "autoscroll",
                "glyph": "⇣",
                "title": "Toggle autoscroll",
                "toggle": True,
            },
        ],
    }


async def _render_html(id, payload, kernel):
    """No args. Returns {html:str} — bundled webapp/index.html, read fresh on each call."""
    return {"html": _bundled_html()}


async def _boot(id, payload, agent):
    """Idempotent first-boot wiring: ensure a terminal_backend child
    exists. If `agent._children` already has a terminal_backend (the
    common case after kernel restart), this is a no-op — the child
    record was rehydrated from disk. On first boot only, we
    `agent.create` it; the substrate persists, registers in
    ctx.agents, and fires the new child's `_boot` (which spawns the
    PTY)."""
    BACKEND_HM = "terminal_backend.tools"
    has_backend = any(c.handler_module == BACKEND_HM for c in agent._children.values())
    if has_backend:
        return None
    rec = agent.create(BACKEND_HM)
    if "error" in rec:
        return rec
    # Track the child id on our own record so domain code (the iframe,
    # canvas frame chrome) can discover the pair without traversing
    # the children dict.
    agent.update(id, upstream_id=rec["id"])
    # Boot the new child via kernel routing — its bundle's _boot fires
    # and spawns the PTY child process.
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
        return {"error": f"terminal_webapp: unknown type {t!r}"}
    return await fn(id, payload, kernel)
