"""ai_chat_webapp — provider-agnostic chat UI fronting any LLM backend.

Holds an `upstream_id` pointing at a backend that answers `send`,
`history`, `interrupt` (and emits `token`/`done`/`queued`/`status`).
ollama_backend, nvidia_nim_backend, and any future LLM backend that
matches that surface all work without changes here.

Real work in `webapp/index.html` (read fresh on every GET).
"""

from __future__ import annotations

from importlib import resources


def _bundled_html() -> str:
    """Read the bundle's index.html from package resources, NOT cached."""
    return (resources.files("ai_chat_webapp") / "webapp" / "index.html").read_text(
        "utf-8"
    )


# ─── verbs ──────────────────────────────────────────────────────


async def _reflect(id, payload, kernel):
    """Identity + upstream_id binding. No args."""
    rec = kernel.get(id) or {}
    return {
        "id": id,
        "sentence": "Chat UI fronting an upstream LLM backend.",
        "upstream_id": rec.get("upstream_id"),
        "verbs": {
            n: (f.__doc__ or "").strip().splitlines()[0] for n, f in VERBS.items()
        },
    }


async def _get_webapp(id, payload, kernel):
    """No args. Canvas-facing UI descriptor: {url, default_width, default_height, title}."""
    return {
        "url": f"/{id}/",
        "default_width": 360,
        "default_height": 480,
        "title": "chat",
    }


async def _render_html(id, payload, kernel):
    """No args. Returns {html:str} — bundled webapp/index.html, read fresh on each call."""
    return {"html": _bundled_html()}


async def _boot(id, payload, agent):
    """Idempotent first-boot wiring: ensure a provider backend child
    exists. The provider is selected by the `provider` field on the
    record (`"ollama"` or `"nvidia_nim"`); default `ollama`. On every
    subsequent boot, the backend record is already present from disk
    and this is a no-op.

    Cascade-delete this chat webapp and its provider backend goes
    with it via the substrate's structural cascade.
    """
    rec = agent.get(id) or {}
    provider = rec.get("provider", "ollama")
    backend_hm = (
        "ollama_backend.tools" if provider == "ollama" else "nvidia_nim_backend.tools"
    )
    has_backend = any(c.handler_module == backend_hm for c in agent._children.values())
    if has_backend:
        return None
    new_rec = agent.create(backend_hm)
    if "error" in new_rec:
        return new_rec
    agent.update(id, upstream_id=new_rec["id"])
    await agent.send(new_rec["id"], {"type": "boot"})
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
        return {"error": f"ai_chat_webapp: unknown type {t!r}"}
    return await fn(id, payload, kernel)
