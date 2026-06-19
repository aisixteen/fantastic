"""ws_bridge — cross-kernel agent comms (WS-only, asymmetric).

A bridge agent on kernel A opens a WS connection to kernel B's `web_ws` endpoint
and ships **raw** bridge frames. A local agent reaches a remote agent with:

    await kernel.send(local_bridge_id, {
        "type": "forward",
        "target": "<remote_agent_id>",
        "payload": {"type": "reflect"},
    })

The bridge sends `{type:'call', id, target, payload}`; B's `web_ws` dispatches it
via `kernel.send(target, payload)` exactly like a browser call, and the matching
`{type:'reply', id, data}` flows back. **No B-side bridge agent needed** — the
bridge is an asymmetric client. Streaming uses `watch_remote` →
`{type:'watch', src}`; `{type:'event'}` frames re-emit on the bridge's own inbox.

This bundle is THIN: the transport-agnostic engine (read loop, verbs, lifecycle,
reply correlation) lives in `io_bridge`; here we only supply the `ws`/`ssh+ws`
transport builder (`_ws.build_transport`) + reflect dressing. The sibling
`relay_connector` bundle reuses the same engine with a relay-router transport.

Transports: `memory` (test backbone) / `ws` / `ssh+ws`.
"""

from __future__ import annotations

from io_bridge import dispatch, make_verbs
from io_bridge import on_delete as _engine_on_delete

# Re-exported so the bundle's tests can inject a MemoryTransport / inspect engine
# state via `ws_bridge.tools.<name>` (the engine lives in io_bridge now).
from io_bridge import (  # noqa: F401
    _bridges,
    _next_corr,
    _state,
    _test_transport_inject,
)
from ws_bridge._ws import SENTENCE, build_transport, reflect_fields

VERBS = make_verbs(
    build_transport=build_transport,
    sentence=SENTENCE,
    reflect_fields=reflect_fields,
    default_kind="ws",
)


async def handler(id: str, payload: dict, kernel) -> dict | None:
    return await dispatch(VERBS, id, payload, kernel)


async def on_delete(agent):
    """Cascade hook — cancels the read loop, closes the transport, kills the SSH
    tunnel (if any), rejects pending Futures. Delegated to the shared engine."""
    return await _engine_on_delete(agent)
