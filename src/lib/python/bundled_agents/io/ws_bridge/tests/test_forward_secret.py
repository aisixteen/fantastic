"""A forwarded `set` is secret in the CALLER's records (its fields are marked by
the bridge's `payload_secret` hook), and a `get` reply's marks ride the reply frame
back so the calling kernel records the reply as `[secret]`. The caller always gets
the value whole."""

from __future__ import annotations

import asyncio

from io_bridge import MemoryTransport
from io_bridge import _engine
from relay_connector import tools as relay_tools
from test_ws_bridge import _make_bridge, _wire_memory_pair
from test_ws_bridge import two_kernels  # noqa: F401  (the fixture, by name below)
from ws_bridge import tools as ws_tools

SENTINEL = "sk-SENTINEL-7f3a"


def _forward(target: str, inner: dict) -> dict:
    return {"type": "forward", "target": target, "payload": inner}


def test_bridge_payload_secret_marks_only_a_forwarded_set_fields():
    for tools in (ws_tools, relay_tools):
        hook = tools.payload_secret
        assert hook(_forward("c", {"type": "set", "fields": {"a": 1, "b": 2}})) == [
            "payload.fields.a",
            "payload.fields.b",
        ]
        # a key holding a dot, or an empty key, marks the whole `payload.fields`
        assert hook(_forward("c", {"type": "set", "fields": {"a.b": 1}})) == [
            "payload.fields"
        ]
        assert hook(_forward("c", {"type": "set", "fields": {"": 1}})) == [
            "payload.fields"
        ]
        # only a forwarded `set` is marked
        assert hook(_forward("c", {"type": "get", "ref": "r"})) == []
        assert hook(_forward("c", {"type": "reflect"})) == []
        assert hook({"type": "set", "fields": {"a": 1}}) == []
        assert hook(_forward("c", {"type": "set", "ref": "r"})) == []


async def test_forward_set_to_a_connected_credential_store_never_records_the_value_on_the_caller(
    request, tmp_path
):
    ka, kb = request.getfixturevalue("two_kernels")
    a_id, b_id = await _wire_memory_pair(ka, kb)
    # both legs may reach the store by name (the operator's can_call / callable_by)
    for k, leg in ((ka, a_id), (kb, b_id)):
        cc = dict(k.ctx.agents[leg]._meta.get("can_call") or {})
        k.update(leg, can_call={**cc, "creds": "*"})
    # the far kernel holds the real value in a credential store with a declared ref
    kb.create(
        "credential_store.tools",
        id="creds",
        file=str(tmp_path / "creds.yaml"),
        callable_by={b_id: "*"},
    )
    await kb.send(
        [
            {
                "target": "kernel",
                "payload": {
                    "type": "update_agent",
                    "id": "creds",
                    "refs": {
                        "svc/main": {"host": "api.example", "fields": ["api_key"]}
                    },
                },
            }
        ]
    )
    events: list[dict] = []
    off = ka.ctx.add_state_subscriber(events.append)
    try:
        out = (
            await ka.send(
                [
                    {
                        "target": a_id,
                        "payload": _forward(
                            "creds",
                            {
                                "type": "set",
                                "ref": "svc/main",
                                "fields": {"api_key": SENTINEL},
                            },
                        ),
                    }
                ]
            )
        )[0]
    finally:
        off()
    assert out["status"] == "completed", out
    # the far store really holds it
    got = (
        await kb.send(
            [{"target": "creds", "payload": {"type": "get", "ref": "svc/main"}}]
        )
    )[0]["result"]
    assert got["fields"]["api_key"] == SENTINEL, got
    # the calling kernel keeps no trace of the value
    ledger = (await ka.send([{"target": "kernel", "payload": {"type": "ledger"}}]))[0]
    assert SENTINEL not in repr(ledger), "ledger holds the value"
    assert SENTINEL not in repr(events), "state stream holds the value"
    assert SENTINEL not in repr([r for r in ka.ctx.rows.values()]), (
        "call rows hold the value"
    )


async def test_forward_get_to_a_connected_credential_store_is_recorded_as_secret(
    request,
):
    ka, kb = request.getfixturevalue("two_kernels")
    near = await _make_bridge(ka, peer_id="far", transport="memory")
    # the far end is the test itself: its frames are scripted here
    mt_near, mt_far = MemoryTransport.pair()
    _engine._test_transport_inject[near] = mt_near
    boot = (await ka.send([{"target": near, "payload": {"type": "boot"}}]))[0]
    assert boot["result"].get("booted") is True, boot

    async def far_reply(secret, data):
        frame = await asyncio.wait_for(mt_far.recv(), 3)
        assert frame["type"] == "call", frame
        reply = {"type": "reply", "id": frame["id"], "data": data}
        if secret is not None:
            reply["secret"] = secret
        await mt_far.send(reply)

    reply_data = {"fields": {"api_key": SENTINEL}}
    events: list[dict] = []
    off = ka.ctx.add_state_subscriber(events.append)
    try:
        fwd = _forward("creds", {"type": "get", "ref": "svc/main"})
        send = asyncio.create_task(
            ka.send([{"target": near, "payload": fwd, "id": "fwd-get-1"}])
        )
        await far_reply(["fields.api_key"], reply_data)
        out = (await asyncio.wait_for(send, 3))[0]
    finally:
        off()
    # the caller gets the value whole
    assert out["result"] == reply_data, out
    assert out["secret"] == ["fields.api_key"], out
    # the calling kernel's settle row (named: it keeps the result) shows [secret]
    settles = [
        r
        for r in ka.ctx.rows.values()
        if r.get("kind") == "settle" and r.get("parent") == out["pid"]
    ]
    assert settles and settles[0]["outcome"]["result"] == {
        "fields": {"api_key": "[secret]"}
    }, settles
    assert SENTINEL not in repr(events), "state stream holds the value"

    # a MALFORMED reply `secret` (a string, not a list of paths) is ignored: the
    # reply is still delivered whole and nothing is marked
    send = asyncio.create_task(
        ka.send([{"target": near, "payload": fwd, "id": "fwd-get-2"}])
    )
    await far_reply("fields.api_key", reply_data)
    out = (await asyncio.wait_for(send, 3))[0]
    assert out["result"] == reply_data, out
    assert "secret" not in out, out
