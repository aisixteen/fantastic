"""ws_bridge — cross-kernel agent comms (WS-only, asymmetric).

A bridge agent on kernel A opens a WS connection to kernel B's `web_ws` endpoint
and ships **raw** bridge frames. A local agent reaches a remote agent with:

    (await kernel.send([{"target": local_bridge_id, "payload": {
        "type": "forward",
        "target": "<remote_agent_id>",
        "payload": {"type": "reflect"},
    }}]))[0]["result"]

The bridge sends `{type:'call', id, target, payload, caller, secret?}` (`secret` =
the forward call's own marks re-rooted at the forwarded item — `io_bridge`'s
`forwarded_secret`); B's `web_ws` dispatches it
via `kernel.send([{"target": target, "payload": payload}])` exactly like a browser call, and the matching
`{type:'reply', id, data}` flows back. **No B-side bridge agent needed** — the
bridge is an asymmetric client. Streaming uses `watch_remote` →
`{type:'watch', src}`; `{type:'event'}` frames re-emit on the bridge's own inbox.

This bundle is THIN: the transport-agnostic engine (read loop, verbs, lifecycle,
reply correlation) lives in `io_bridge`; here we only supply the `ws`
transport builder (`_ws.build_transport`) + reflect dressing. The sibling
`relay_connector` bundle reuses the same engine with a relay-router transport.

Transports: `memory` (test backbone) / `ws`. A tunnel is not this bundle's job —
an ssh tunnel is an `exec_bridge` job (or devops); the bridge dials its local end.
"""

from __future__ import annotations

from io_bridge import _bridges, dispatch, make_verbs
from io_bridge import on_delete as _engine_on_delete
from io_bridge import validate_meta  # noqa: F401  (the `peer_guid` write check)
from kernel import current_secret
from ws_bridge._ws import (
    SENTENCE,
    WSTransport,
    build_transport,
    reflect_fields,
    secret_sealed,
)

VERBS = make_verbs(
    build_transport=build_transport,
    sentence=SENTENCE,
    reflect_fields=reflect_fields,
    default_kind="ws",
)

_engine_forward = VERBS["forward"]


async def _forward(id, payload, kernel):
    # THE WS SEAL, outbound: a `forward` whose own call carries ANY secret mark,
    # on a leg dialed to a non-loopback host, is refused before a frame is built.
    # `forward` is the one outbound call path a ws leg has — no ws leg registers a
    # `PeerRoute` (only web_ws registers peers).
    # TODO(ws seal): wss:// (TLS) is the later path — a TLS leg carries secrets
    # to any host, and this seal lifts for it.
    st = _bridges.get(id)
    if (
        current_secret()
        and st is not None
        and isinstance(st.transport, WSTransport)
        and st.transport.sealed
    ):
        return secret_sealed()
    return await _engine_forward(id, payload, kernel)


_forward.__doc__ = (
    (_engine_forward.__doc__ or "")
    + " SEALED for secrets to a non-loopback host: a call carrying any secret mark "
    "is refused — ws:// carries every frame in clear."
)
VERBS["forward"] = _forward


async def handler(id: str, payload: dict, kernel) -> dict | None:
    return await dispatch(VERBS, id, payload, kernel)


async def on_delete(agent):
    """Cascade hook — cancels the read loop, closes the transport, rejects
    pending Futures. Delegated to the shared engine."""
    return await _engine_on_delete(agent)


# Shutdown frees the same process resources — declared, never inferred.
on_shutdown = on_delete
