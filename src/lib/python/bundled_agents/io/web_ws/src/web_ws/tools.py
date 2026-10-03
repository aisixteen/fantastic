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
  get_routes    -> the duck-typed call surface used by `web._boot`; PURE DATA
                   ({kind, path}), so the reply renders over REST and persists
                   into the ledger like any other. The live endpoint callable
                   is handed to the parent in-process through the module hook
                   `route_endpoints(agent)` below — never through a reply.
  inbound_call / inbound_emit / inbound_watch / inbound_state_subscribe
                -> the INBOUND half (`io_bridge._inbound`): the socket loop hands
                   each admitted frame to this leg as one of these, and the leg
                   performs it AS ITSELF — gated as the leg.
"""

from __future__ import annotations

from fastapi import WebSocket

from io_bridge import describe as _describe
from io_bridge import (
    inbound_call,
    inbound_emit,
    inbound_state_subscribe,
    inbound_watch,
    unknown_ingress_refusal,
)
from web_ws._proxy import run as _proxy_run
from kernel._refusal import UNHANDLED, refusal


# ─── route endpoint ─────────────────────────────────────────────


def _make_endpoint(self_id: str, kernel):
    async def _ws_endpoint(websocket: WebSocket, host_id: str):
        await websocket.accept()
        # `web_agent_id` is the surface's own id (this web_ws instance): every
        # admitted frame is handed to it, and it acts as itself.
        await _proxy_run(websocket, kernel, host_id, self_id)

    return _ws_endpoint


PATH_PATTERN = "/{host_id}/ws"


def route_endpoints(agent):
    """Duck-typed module hook the parent `web` host calls at mount time
    (discovered exactly like `on_delete` / `reflect_root_extra`): `{path: live
    endpoint}` for every path `get_routes` declared. The callable is an
    in-process object — it travels this way, NOT inside a verb reply, so the
    reply stays JSON."""
    return {PATH_PATTERN: _make_endpoint(agent.id, agent)}


# ─── verbs ──────────────────────────────────────────────────────


async def _reflect(id, payload, kernel):
    """Identity + the URL pattern this surface mounts + this leg's auth posture
    (`ingress`/`egress`/`sealed`/`see`). No args. A sealed leg surfaces the
    `see` pointer (→ the `io_bridge` agent) so a denied client learns how to open the edge."""
    return {
        "id": id,
        "sentence": "WS verb-invocation surface; mounts /<host_id>/ws on the parent web.",
        "path_pattern": PATH_PATTERN,
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
    """Declares this surface's HTTP/WS route mounts. No args. Returns
    {routes:[{kind:'websocket', path}]} — pure data. The parent `web` host gets
    the live endpoint for each path from this module's `route_endpoints(agent)`
    hook, in-process."""
    return {"routes": [{"kind": "websocket", "path": PATH_PATTERN}]}


# ─── dispatch ───────────────────────────────────────────────────


VERBS = {
    "reflect": _reflect,
    "boot": _boot,
    "get_routes": _get_routes,
    "inbound_call": inbound_call,
    "inbound_emit": inbound_emit,
    "inbound_watch": inbound_watch,
    "inbound_state_subscribe": inbound_state_subscribe,
}


def validate_meta(meta: dict, record: dict) -> str | None:
    """The leg's `ingress_rule`, refused at the WRITE when no rule implements it
    — the one check every leg makes (`io_bridge.unknown_ingress_refusal`)."""
    return unknown_ingress_refusal(meta)


async def handler(id: str, payload: dict, kernel) -> dict | None:
    t = payload.get("type")
    fn = VERBS.get(t)
    if fn is None:
        return refusal(UNHANDLED, f"web_ws: unknown type {t!r}", verb=t)
    return await fn(id, payload, kernel)
