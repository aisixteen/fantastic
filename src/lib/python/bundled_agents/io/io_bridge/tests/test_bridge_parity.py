"""Python twins of Rust tests that span BOTH bridge families (ws_bridge and
relay_connector, one `io_bridge` engine) (#170): each is named
`test_<rust test name>`. Python is the spec."""

from __future__ import annotations

import json

import pytest

from io_bridge import _engine
from relay_connector import tools as rc
from ws_bridge import tools as wb


@pytest.fixture(autouse=True)
def _clean_bridges():
    yield
    _engine._bridges.clear()
    _engine._test_transport_inject.clear()


async def _op(k, payload, target=None) -> dict:
    return (await k.send([{"target": target or k.id, "payload": payload}]))[0]["result"]


async def _leg(k, module: str, **meta) -> str:
    rec = await _op(
        k,
        {
            "type": "create_agent",
            "parent_id": k.id,
            "handler_module": module,
            **meta,
        },
    )
    assert rec.get("id"), rec
    return rec["id"]


async def test_reflect_key_order_per_family(kernel):
    ws = await _leg(kernel, "ws_bridge.tools")
    relay = await _leg(kernel, "relay_connector.tools")

    r = await _op(kernel, {"type": "reflect"}, ws)
    # (the kernel appends its own gate keys after the bundle's body)
    assert list(r)[:11] == [
        "id",
        "sentence",
        "transport",
        "connected",
        "pending_count",
        "ingress_rule",
        "egress_rule",
        "sealed",
        "host",
        "peer_id",
        "port",
    ]
    assert r["sentence"] == (
        "Cross-kernel comms bridge — WS-only, asymmetric (no peer bridge needed); "
        "weak proxy."
    )
    assert r["transport"] == "ws", "the ws default kind"
    assert r["connected"] is False
    assert r["sealed"] is True
    assert len(wb.VERBS) == 6
    assert (
        wb.VERBS["reflect"].__doc__ == "Identity + transport + connectivity. No args."
    )
    assert wb.EMITS["bridge_down"] == (
        "{type:'bridge_down'} — emitted when the transport drops (peer closed, "
        "tunnel died)"
    )

    r = await _op(kernel, {"type": "reflect"}, relay)
    assert list(r)[:15] == [
        "id",
        "sentence",
        "transport",
        "connected",
        "pending_count",
        "ingress_rule",
        "egress_rule",
        "sealed",
        "relay_url",
        "guid",
        "partner_guid",
        "reconnect",
        "role",
        "owner_guid",
        "exposes",
    ]
    assert r["sentence"] == (
        "Cross-kernel comms through a relay-kernel router — dial-out WS, "
        "group-password auth (X-Fantastic-Auth), GUID-routed; symmetric RPC "
        "tunneled over the relay; auto-reconnecting."
    )
    assert r["transport"] == "relay", "the relay default kind"
    assert r["reconnect"] == 10.0 and isinstance(r["reconnect"], float)
    assert r["role"] == "kernel"
    assert r["exposes"] == []
    assert len(rc.VERBS) == 10
    assert rc.VERBS["unwatch_directory"].__doc__ == (
        "No args. Stop the directory subscription started by `watch_directory`."
    )
    assert rc.EMITS["bridge_down"] == wb.EMITS["bridge_down"]


async def test_unknown_type_wire_shape_is_byte_exact_on_both_families(kernel):
    ws = await _leg(kernel, "ws_bridge.tools")
    relay = await _leg(
        kernel, "relay_connector.tools", transport="relay", relay_url="ws://127.0.0.1:1"
    )
    hint = (
        'every bridge frame needs a \\"type\\"; to reach a remote agent use '
        '{\\"type\\":\\"forward\\",\\"target\\":\\"<remote_id>\\",\\"payload\\":'
        '{\\"type\\":\\"<verb>\\",...}} — the inner payload is itself a verb frame '
        'and needs its own \\"type\\".'
    )

    def wire(r) -> str:
        return json.dumps(r, ensure_ascii=False, separators=(",", ":"))

    for leg in (ws, relay):
        r = await _op(kernel, {"type": "garbage"}, leg)
        assert wire(r) == (
            '{"error":"bridge: unknown type \'garbage\'","reason":"unhandled",'
            f'"hint":"{hint}","verb":"garbage"}}'
        ), leg
    # a payload with no `type` (the handler, past the kernel's own payload check)
    r = await wb.handler(ws, {}, kernel)
    assert wire(r) == (
        '{"error":"bridge: unknown type None","reason":"unhandled",'
        f'"hint":"{hint}","verb":null}}'
    )
