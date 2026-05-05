"""telemetry_pane — live agent-vis GL view for canvas hosts.

Answers `get_gl_view`. The host (canvas_webapp) compiles the returned
JS source via `new Function('THREE','scene','t','onFrame','cleanup',
source)` and runs it inside its WebGL scene. The source subscribes
to the kernel state stream via `t.subscribeState` and renders each
agent as a Three.js Sprite with its display_name, a 10-dot backlog
indicator (`+N more` overflow), and a brief border flash on each
send/emit. `cleanup.push(...)` registers teardown closures for
proper disposal on `remove_agent`.

The render path is a pure consumer of the substrate — no
`kernel.send`/`emit`/`call` from inside it — so even an instance
that visualizes itself does not feedback-loop. The bundle's tests
include a drift guard asserting the source contains no kernel
calls.
"""

from __future__ import annotations

from importlib import resources


def _bundled_glview() -> str:
    """Read this bundle's glview.js fresh each call (no caching)."""
    return (resources.files("telemetry_pane") / "glview.js").read_text("utf-8")


# ─── verbs ──────────────────────────────────────────────────────


async def _reflect(id, payload, kernel):
    """Identity + verbs. No args."""
    return {
        "id": id,
        "sentence": "Live agent visualization GL view (canvas peer).",
        "verbs": {
            n: (f.__doc__ or "").strip().splitlines()[0] for n, f in VERBS.items()
        },
    }


async def _get_gl_view(id, payload, kernel):
    """No args. Returns {source:str, title:str} for the canvas's GL host. The source is a function body run inside `new Function('THREE','scene','t','onFrame','cleanup', source)`. It must push teardown closures into `cleanup` and must NOT call kernel verbs from inside the render path (no `t.call` / `t.send` / `t.emit`)."""
    return {"source": _bundled_glview(), "title": "telemetry"}


async def _boot(id, payload, kernel):
    """No-op. Returns None."""
    return None


# ─── dispatch ───────────────────────────────────────────────────


VERBS = {
    "reflect": _reflect,
    "get_gl_view": _get_gl_view,
    "boot": _boot,
}


async def handler(id: str, payload: dict, kernel) -> dict | None:
    t = payload.get("type")
    fn = VERBS.get(t)
    if fn is None:
        return {"error": f"telemetry_pane: unknown type {t!r}"}
    return await fn(id, payload, kernel)
