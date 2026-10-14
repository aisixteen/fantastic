"""relay_connector — cross-kernel agent comms through a relay-KERNEL router.

A connector agent on kernel A dials the relay (`../fantastic_relay`) at
`ws://<host>/<guid>` (group password in `X-Fantastic-Auth` — read from a
credential_store before every dial, never kept on the record — `fantastic.relay.v1`
subprotocol) and reaches a partner kernel B — which runs its OWN connector — by
B's GUID. A local agent reaches a remote agent exactly as with `ws_bridge`:

    (await kernel.send([{"target": local_connector_id, "payload": {
        "type": "forward",
        "target": "<remote_agent_id>",
        "payload": {"type": "reflect"},
    }}]))[0]["result"]

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
dressing. Record fields: `relay_url` · `guid` · `partner_guid` · `credentials`
({store, ref} — the relay password, field `password`, bound to the relay's host)
· `heartbeat`. Per-leg `ingress_rule`/`egress_rule`/`auth`
gate the tunneled bridge calls (sealed by default), independent of the relay's
own connection auth.

Transport: `relay`.
"""

from __future__ import annotations

from io_bridge import _state, dispatch, make_verbs
from io_bridge import on_delete as _engine_on_delete
from io_bridge import EMITS  # noqa: F401  (`docs` tier detailed)
from io_bridge import validate_meta as _bridge_validate_meta
from kernel._refusal import NOT_CONNECTED, bad, refusal
from relay_connector._relay import (
    DIRECTORY_SRC,
    ROLES,
    SENTENCE,
    _identity_from_record,
    build_transport,
    credentials_shape_error,
    reflect_fields,
)


def validate_meta(meta: dict, record: dict) -> str | None:
    """The record's own fields, refused at the write: `credentials` ({store, ref})
    — the relay password lives in a credential_store
    — then the bridge's `ingress_rule` check."""
    if meta.get("credentials") is not None and (
        bad := credentials_shape_error(meta["credentials"])
    ):
        return bad
    return _bridge_validate_meta(meta, record)


VERBS = make_verbs(
    build_transport=build_transport,
    sentence=SENTENCE,
    reflect_fields=reflect_fields,
    default_kind="relay",
)

# ── directory surface (relay-specific; the relay's own `relay` agent) ──


async def _list_peers(id, payload, kernel):
    """args: list_timeout:float? (default 30s — not `timeout`, which is a call's own routing field). One-shot snapshot of the relay directory:
    `{peers:[{guid, status(green|yellow|red), last_seen, since}]}` — addresses the
    relay's own `relay` agent (target:"relay"), not the partner. Boot first."""
    st = _state(id)
    if st.transport is None or st.transport.closed:
        return refusal(
            NOT_CONNECTED,
            "relay_connector.list_peers: not connected",
            hint="call boot first",
        )
    return await st.transport.list_peers(
        timeout=float(payload.get("list_timeout", 30.0))
    )


async def _watch_directory(id, payload, kernel):
    """No args. Subscribe to the relay directory; live `peer_joined`/`peer_left`/
    `peer_evicted`/`peer_status` events re-emit on THIS connector's inbox — a local
    watcher (`kernel.watch(<connector_id>, ...)`) renders the green/yellow/red list
    without polling."""
    st = _state(id)
    if st.transport is None or st.transport.closed:
        return refusal(
            NOT_CONNECTED,
            "relay_connector.watch_directory: not connected",
            hint="call boot first",
        )
    # The leg SUBSCRIBES to the directory as `watch_remote` does to a remote
    # agent — before asking: the read loop may hand the engine an event that
    # follows the relay's ack before this verb resumes. A refusal withdraws it.
    st.watching.add(DIRECTORY_SRC)
    out = await st.transport.watch_directory()
    if out.get("error"):
        st.watching.discard(DIRECTORY_SRC)
    return out


async def _unwatch_directory(id, payload, kernel):
    """No args. Stop the directory subscription started by `watch_directory`."""
    st = _state(id)
    st.watching.discard(DIRECTORY_SRC)
    if st.transport is None or st.transport.closed:
        return {"ok": True}
    return await st.transport.unwatch_directory()


async def _set_identity(id, payload, kernel):
    """Advertise/update this peer's DIRECTORY identity — the typed attrs the relay
    reflects into `list_peers` + a `peer_updated` event (it never interprets them).
    Well-known keys (all optional, merged over the record): `role` (manager|kernel),
    `owner_guid` (the managing peer; null = standalone), `exposes` (control-surface
    list). Persists the change so it re-announces on the next boot. Boot first."""
    st = _state(id)
    if st.transport is None or st.transport.closed:
        return refusal(
            NOT_CONNECTED,
            "relay_connector.set_identity: not connected",
            hint="call boot first",
        )
    if (
        "role" in payload
        and payload["role"] is not None
        and payload["role"] not in ROLES
    ):
        return bad("set_identity", "role", f"one of {ROLES}")
    # Merge only the provided well-known keys into the record + persist (an explicit
    # null retracts a field, e.g. owner_guid:null = become standalone).
    patch = {k: payload[k] for k in ("role", "owner_guid", "exposes") if k in payload}
    if patch:
        kernel.update(id, **patch)
    attrs = _identity_from_record(kernel.get(id) or {})
    return await st.transport.set_identity(attrs)


VERBS["list_peers"] = _list_peers
VERBS["watch_directory"] = _watch_directory
VERBS["unwatch_directory"] = _unwatch_directory
VERBS["set_identity"] = _set_identity


async def handler(id: str, payload: dict, kernel) -> dict | None:
    return await dispatch(VERBS, id, payload, kernel)


async def on_delete(agent):
    """Cascade hook — cancels the read loop + heartbeat, closes the relay
    connection, rejects pending Futures. Delegated to the shared engine."""
    return await _engine_on_delete(agent)


# Shutdown frees the same process resources — declared, never inferred.
on_shutdown = on_delete
