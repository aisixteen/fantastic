"""web_ws — WebSocket verb-invocation surface as a sub-agent of `web`.

Lives as a child of a `web` agent. Declares one route via the
duck-typed `get_routes` verb: a WebSocket endpoint at
`/{host_id}/ws` that any browser / ws_bridge / curl-WS client
opens to invoke verbs (`call` frames) and stream events (`watch` /
`state_subscribe` frames).

The WS frame protocol lives in this bundle's OWN `_proxy.run` — web_ws
both declares the route AND owns the frame loop (no reach into `web`),
so a `web` instance can run rendering-only and multiple call surfaces
(REST too) coexist as siblings.

Verbs:
  reflect       -> identity + endpoint URL pattern
  boot          -> no-op (web mounts the route by pulling get_routes)
  get_routes    -> the duck-typed call surface used by `web._boot`
"""

from __future__ import annotations

from fastapi import WebSocket

from io_bridge import describe as _describe
from web_ws._proxy import run as _proxy_run


# ─── route endpoint ─────────────────────────────────────────────


def _make_endpoint(self_id: str, kernel):
    async def _ws_endpoint(websocket: WebSocket, host_id: str):
        await websocket.accept()
        # `web_agent_id` is the surface's own id (this web_ws instance).
        # The proxy uses it to tag external traffic via _current_sender,
        # so telemetry rays originate from the surface's sprite.
        await _proxy_run(websocket, kernel, host_id, self_id)

    return _ws_endpoint


# ─── verbs ──────────────────────────────────────────────────────


async def _reflect(id, payload, kernel):
    """Identity + the URL pattern this surface mounts + this leg's auth posture
    (`ingress`/`egress`/`sealed`/`see`). No args. A sealed leg surfaces the
    `see` pointer (→ the `io_bridge` agent) so a denied client learns how to open the edge."""
    return {
        "id": id,
        "sentence": "WS verb-invocation surface; mounts /<host_id>/ws on the parent web.",
        "path_pattern": "/{host_id}/ws",
        # The leg is a CHANNEL — surface its auth posture so discovery-through-denial
        # has something to discover (the door + the pointer to how to open it).
        **_describe(kernel.get(id) or {}),
        "verbs": {
            n: (f.__doc__ or "").strip().splitlines()[0] for n, f in VERBS.items()
        },
    }


async def _boot(id, payload, kernel):
    """No-op. The parent `web` agent calls `get_routes` on this bundle during its own boot and mounts the WS endpoint onto its FastAPI app."""
    return None


async def _get_routes(id, payload, kernel):
    """Declares this surface's HTTP/WS route mounts. Returns
    {routes:[{kind:'websocket', path, endpoint}]}. `endpoint` is an
    async callable accepting (websocket, host_id) — same shape FastAPI
    expects for `app.add_api_websocket_route(path, endpoint)`."""
    return {
        "routes": [
            {
                "kind": "websocket",
                "path": "/{host_id}/ws",
                "endpoint": _make_endpoint(id, kernel),
            }
        ]
    }


# ─── dispatch ───────────────────────────────────────────────────


VERBS = {
    "reflect": _reflect,
    "boot": _boot,
    "get_routes": _get_routes,
}


async def handler(id: str, payload: dict, kernel) -> dict | None:
    t = payload.get("type")
    fn = VERBS.get(t)
    if fn is None:
        return {"error": f"web_ws: unknown type {t!r}"}
    return await fn(id, payload, kernel)
