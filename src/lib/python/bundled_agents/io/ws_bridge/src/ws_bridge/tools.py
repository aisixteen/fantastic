"""ws_bridge — cross-kernel agent comms (WS-only, asymmetric).

A bridge agent on kernel A opens a WS connection to kernel B's `web_ws` endpoint
and ships **raw** bridge frames. A local agent reaches a remote agent with:

    (await kernel.send([{"target": local_bridge_id, "payload": {
        "type": "forward",
        "target": "<remote_agent_id>",
        "payload": {"type": "reflect"},
    }}]))[0]["result"]

The bridge sends `{type:'call', id, target, payload}`; B's `web_ws` dispatches it
as ITS leg exactly like a browser call, and the matching
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

from io_bridge import dispatch, make_verbs
from io_bridge import on_delete as _engine_on_delete
from io_bridge import EMITS  # noqa: F401  (`docs` tier detailed)
from io_bridge import forward_payload_secret
from io_bridge import validate_meta  # noqa: F401  (the `ingress_rule` write check)
from ws_bridge._ws import SENTENCE, build_transport, reflect_fields

VERBS = make_verbs(
    build_transport=build_transport,
    sentence=SENTENCE,
    reflect_fields=reflect_fields,
    default_kind="ws",
)


async def handler(id: str, payload: dict, kernel) -> dict | None:
    return await dispatch(VERBS, id, payload, kernel)


def payload_secret(payload: dict) -> list[str]:
    """A forwarded `set`'s fields are secret in THIS kernel's records (the call row,
    the ledger, the state stream): `payload.fields.<name>` per field, or the whole
    `payload.fields` for a name that is empty or dotted (`io_bridge`)."""
    return forward_payload_secret(payload)


async def on_delete(agent):
    """Cascade hook — cancels the read loop, closes the transport, rejects
    pending Futures. Delegated to the shared engine."""
    return await _engine_on_delete(agent)


# Shutdown frees the same process resources — declared, never inferred.
on_shutdown = on_delete
