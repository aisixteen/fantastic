"""relay_connector — cross-kernel agent comms through a relay-KERNEL router.

A connector agent on kernel A dials the relay (`../fantastic_relay`) at
`ws://<host>/<guid>` (group password in `X-Fantastic-Auth`, `fantastic.relay.v1`
subprotocol) and reaches a partner kernel B — which runs its OWN connector — by
B's GUID. A local agent reaches a remote agent exactly as with `ws_bridge`:

    await kernel.send(local_connector_id, {
        "type": "forward",
        "target": "<remote_agent_id>",
        "payload": {"type": "reflect"},
    })

The relay routes by `target` and delivers peer→peer as ONE-WAY
`{type:"event", source, payload}` (no relay-level reply). So the connector
TUNNELS the shared `io_bridge` bridge frames (`call`/`reply`/`watch`/`event`)
inside relay `send` frames addressed to `partner_guid`; the engine's read loop on
B (symmetric — both run a connector) dispatches `kernel.send` and tunnels the
reply back. forward/reply correlation, `watch_remote`, and binary `read_stream`
streaming all work UNCHANGED — the engine is the same machine `ws_bridge` uses.

This bundle is THIN: the transport-agnostic engine (read loop, the 6 verbs,
lifecycle, reply correlation, the ingress/egress gate) lives in `io_bridge`; here
we supply only the relay transport builder (`_relay.build_transport`) + reflect
dressing. Record fields: `relay_url` · `guid` · `partner_guid` · `relay_token`
(X-Fantastic-Auth) · `heartbeat`. Per-leg `ingress_rule`/`egress_rule`/`auth`
gate the tunneled bridge calls (sealed by default), independent of the relay's
own connection auth.

Transport: `relay`.
"""

from __future__ import annotations

from io_bridge import dispatch, make_verbs
from io_bridge import on_delete as _engine_on_delete

# Re-exported so the bundle's tests can inject a transport / inspect engine state
# via `relay_connector.tools.<name>` (the engine lives in io_bridge now).
from io_bridge import (  # noqa: F401
    _bridges,
    _next_corr,
    _state,
    _test_transport_inject,
)
from relay_connector._relay import SENTENCE, build_transport, reflect_fields

VERBS = make_verbs(
    build_transport=build_transport,
    sentence=SENTENCE,
    reflect_fields=reflect_fields,
    default_kind="relay",
)


# ── directory surface (relay-specific; the relay's own `relay` agent) ──


async def _list_peers(id, payload, kernel):
    """No args (optional timeout). One-shot snapshot of the relay directory:
    `{peers:[{guid, status(green|yellow|red), last_seen, since}]}` — addresses the
    relay's own `relay` agent (target:"relay"), not the partner. Boot first."""
    st = _state(id)
    if st.transport is None or st.transport.closed:
        return {"error": "relay_connector.list_peers: not connected (call boot first)"}
    return await st.transport.list_peers(timeout=float(payload.get("timeout", 30.0)))


async def _watch_directory(id, payload, kernel):
    """No args. Subscribe to the relay directory; live `peer_joined`/`peer_left`/
    `peer_evicted`/`peer_status` events re-emit on THIS connector's inbox — a local
    watcher (`kernel.watch(<connector_id>, ...)`) renders the green/yellow/red list
    without polling."""
    st = _state(id)
    if st.transport is None or st.transport.closed:
        return {
            "error": "relay_connector.watch_directory: not connected (call boot first)"
        }
    return await st.transport.watch_directory()


async def _unwatch_directory(id, payload, kernel):
    """No args. Stop the directory subscription started by `watch_directory`."""
    st = _state(id)
    if st.transport is None or st.transport.closed:
        return {"ok": True}
    return await st.transport.unwatch_directory()


VERBS["list_peers"] = _list_peers
VERBS["watch_directory"] = _watch_directory
VERBS["unwatch_directory"] = _unwatch_directory


async def handler(id: str, payload: dict, kernel) -> dict | None:
    return await dispatch(VERBS, id, payload, kernel)


async def on_delete(agent):
    """Cascade hook — cancels the read loop + heartbeat, closes the relay
    connection, rejects pending Futures. Delegated to the shared engine."""
    return await _engine_on_delete(agent)
