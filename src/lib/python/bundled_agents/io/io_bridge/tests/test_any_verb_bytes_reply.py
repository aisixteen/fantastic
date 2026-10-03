"""ANY verb whose reply carries raw bytes answers on the binary channel.

There is no list of binary verbs. A bundle's own verb — here `events`, the shape
of a terminal-output poll — is called by a TEXT call frame (no bytes in the
call); its reply holds raw bytes, so every leg answers it binary, found by the
codec (`find_bytes_path`) and nothing else:

  (a) web_ws: a BINARY reply frame, the body at its `_binary_path` array;
      the same verb answering no bytes is a TEXT frame;
  (b) web_rest: an octet reply, the field named by `X-Fantastic-Bytes-Field`,
      the rest of the reply on `X-Fantastic-Payload`; no bytes ⇒ JSON;
  (c) a ws_bridge `forward` through web_ws: the call crosses as TEXT, the
      reply as BINARY, and `forward` returns the bytes;
  (d) a peer kernel's bridge ENGINE serving the inbound call (host web_rest →
      `P-g|probe` over the host's web_ws): text call out, binary reply back,
      octet out of REST;
  (e) bytes at a NESTED key: web_ws carries the full path; web_rest's octet
      names ONE top-level field, so a nested body has no REST form and is
      refused as a stated transport error (500).
"""

from __future__ import annotations

import asyncio
import json
import struct
import types

import httpx
import pytest
from fastapi import FastAPI
from fastapi.testclient import TestClient
from starlette.websockets import WebSocketState

from _testkit import boot_root
from io_bridge import (
    ConnectionClosed,
    MemoryTransport,
    _bridges,
    _test_transport_inject,
    decode_frame,
    encode_frame,
)
from web.app import make_app
from web_rest.tools import _make_post_endpoint
from web_ws._proxy import run as ws_run
from web_ws.tools import _make_endpoint
from kernel import register_unlisted

# ─── the probe: a bundle's own verbs, nothing declared ──────────────────────

CHUNK = b"\x1b[31mred\x1b[0m\x00\xff\r\n"

_calls: list[dict] = []


async def _probe(id, payload, kernel):
    verb = payload.get("type")
    if verb in ("boot", "shutdown"):
        return None
    if verb == "reflect":
        return {"id": id, "sentence": "bytes-reply probe", "verbs": {}}
    _calls.append(payload)
    if verb == "events":
        return {"seq": 3, "chunk": CHUNK}
    if verb == "idle":
        return {"seq": 3}
    if verb == "nested":
        return {"seq": 3, "out": {"chunk": CHUNK}}
    if verb == "listed":
        return {"events": [{"n": 0}, {"n": 1, "bytes": CHUNK}]}
    return {"error": f"probe: no verb {verb!r}"}


_mod = types.ModuleType("anyverb_bytes_probe.tools")
_mod.handler = _probe
_mod.VERBS = {}
register_unlisted("anyverb_bytes_probe.tools", _mod)


def _split(wire: bytes) -> tuple[dict, bytes]:
    """A binary frame → (its JSON header, its raw body)."""
    (n,) = struct.unpack(">I", wire[:4])
    return json.loads(wire[4 : 4 + n]), wire[4 + n :]


# ─── (a) web_ws, (b) web_rest, (e) nested — one kernel ──────────────────────


@pytest.fixture
def one(seeded_kernel):
    _calls.clear()
    seeded_kernel.create(
        "web_ws.tools",
        id="t_ws",
        ingress_rule="allow_all_ingress",
        can_call={"probe": "*"},
    )
    seeded_kernel.create(
        "web_rest.tools",
        id="t_rest",
        ingress_rule="allow_all_ingress",
        can_call={"probe": "*"},
    )
    seeded_kernel.create(
        "anyverb_bytes_probe.tools",
        id="probe",
        callable_by={"t_ws": "*", "t_rest": "*"},
    )
    # `port`: the authority the web's Host guard admits (audit W-2).
    seeded_kernel.create(None, id="t_web", port=8088)
    app = make_app("t_web", seeded_kernel)
    app.add_api_websocket_route("/{host_id}/ws", _make_endpoint("t_ws", seeded_kernel))
    app.add_api_route(
        "/t_rest/{target_id}",
        _make_post_endpoint("t_rest", seeded_kernel),
        methods=["POST"],
    )
    with TestClient(app, headers={"host": "127.0.0.1:8088"}) as c:
        yield c


def _ws_call(ws, fid: str, payload: dict) -> dict:
    """Send a TEXT call frame; return the raw reply message for `fid` (an
    auto-watch `event` may precede it)."""
    ws.send_text(
        json.dumps({"type": "call", "id": fid, "target": "probe", "payload": payload})
    )
    for _ in range(12):
        msg = ws.receive()
        raw = msg.get("text") if msg.get("text") is not None else msg.get("bytes")
        f = decode_frame(raw)
        if f.get("type") == "reply" and f.get("id") == fid:
            return msg
    raise AssertionError(f"no reply for {fid}")


def test_a_web_ws_custom_verb_text_call_gets_a_binary_reply_frame(one):
    with one.websocket_connect("/kernel_state/ws") as ws:
        msg = _ws_call(ws, "e1", {"type": "events"})
        assert msg.get("text") is None, msg
        head, body = _split(msg["bytes"])
        assert head == {
            "type": "reply",
            "id": "e1",
            "data": {"seq": 3, "chunk": None},
            "_binary_path": ["data", "chunk"],
        }
        assert body == CHUNK

        # the SAME verb with no bytes in its reply: a plain TEXT frame
        msg = _ws_call(ws, "e2", {"type": "idle"})
        assert msg.get("bytes") is None, msg
        assert json.loads(msg["text"]) == {
            "type": "reply",
            "id": "e2",
            "data": {"seq": 3},
        }
    assert [c["type"] for c in _calls] == ["events", "idle"]


@pytest.mark.parametrize(
    "verb, data, path",
    [
        ("nested", {"seq": 3, "out": {"chunk": None}}, ["data", "out", "chunk"]),
        (
            "listed",
            {"events": [{"n": 0}, {"n": 1, "bytes": None}]},
            ["data", "events", 1, "bytes"],
        ),
    ],
)
def test_e_web_ws_carries_bytes_at_any_depth(one, verb, data, path):
    with one.websocket_connect("/kernel_state/ws") as ws:
        msg = _ws_call(ws, "n1", {"type": verb})
        head, body = _split(msg["bytes"])
        assert head == {"type": "reply", "id": "n1", "data": data, "_binary_path": path}
        assert body == CHUNK


def test_b_web_rest_custom_verb_json_call_gets_an_octet_reply(one):
    r = one.post("/t_rest/probe", json={"type": "events"})
    assert r.status_code == 200
    assert r.headers["content-type"] == "application/octet-stream"
    assert r.headers["X-Fantastic-Bytes-Field"] == "chunk"
    assert json.loads(r.headers["X-Fantastic-Payload"]) == {"seq": 3}
    assert r.content == CHUNK

    # no bytes in the reply ⇒ plain JSON, no octet headers
    r = one.post("/t_rest/probe", json={"type": "idle"})
    assert r.status_code == 200
    assert r.headers["content-type"] == "application/json"
    assert "X-Fantastic-Bytes-Field" not in r.headers
    assert "X-Fantastic-Payload" not in r.headers
    assert r.json() == {"seq": 3}


@pytest.mark.parametrize("verb", ["nested", "listed"])
def test_e_web_rest_bytes_below_the_top_level_have_no_rest_form(one, verb):
    """The octet reply names ONE top-level field (`X-Fantastic-Bytes-Field` is a
    literal key, never a path), so bytes below the top level cannot ride it, and
    JSON has no bytes: the leg answers its stated transport refusal."""
    r = one.post("/t_rest/probe", json={"type": verb})
    assert r.status_code == 500
    assert r.headers["content-type"] == "application/json"
    assert r.json() == {
        "error": "web_rest: the reply has no JSON representation on this leg",
        "reason": "transport_error",
        "hint": (
            "this verb answers with in-process objects; call it in-process, not "
            "over HTTP"
        ),
    }
    assert [c["type"] for c in _calls] == [verb]


# ─── two kernels ─────────────────────────────────────────────────────────────


async def _op(k, target, payload):
    return (await k.send([{"target": target, "payload": payload}]))[0]


def _kernel(path, monkeypatch, guid: str):
    path.mkdir()
    monkeypatch.chdir(path)
    k = boot_root()
    k.update(k.id, guid=guid)
    return k


@pytest.fixture
async def two(tmp_path, monkeypatch):
    _calls.clear()
    ka = _kernel(tmp_path / "a", monkeypatch, "A-g")
    kb = _kernel(tmp_path / "b", monkeypatch, "B-g")
    monkeypatch.chdir(tmp_path)
    yield ka, kb
    _bridges.clear()
    _test_transport_inject.clear()


class _WsEnd:
    """A web_ws socket whose far end is a bridge leg's MemoryTransport half;
    every frame crosses the REAL codec. `wire` keeps each frame as it crossed:
    (direction, "text" | "binary", the frame) — direction `in` is toward web_ws."""

    def __init__(self, mt: MemoryTransport):
        self.application_state = WebSocketState.CONNECTED
        self.mt = mt
        self.wire: list[tuple[str, str, object]] = []

    async def receive(self):
        try:
            frame = await self.mt.recv()
        except ConnectionClosed:
            return {"type": "websocket.disconnect"}
        raw, is_bin = encode_frame(frame)
        if is_bin:
            self.wire.append(("in", "binary", _split(raw)))
            return {"type": "websocket.receive", "bytes": raw}
        self.wire.append(("in", "text", json.loads(raw)))
        return {"type": "websocket.receive", "text": raw.decode("utf-8")}

    async def send_text(self, text):
        self.wire.append(("out", "text", json.loads(text)))
        await self.mt.send(decode_frame(text))

    async def send_bytes(self, data):
        self.wire.append(("out", "binary", _split(data)))
        await self.mt.send(decode_frame(data))


async def _until(pred, secs=3.0):
    loop = asyncio.get_running_loop()
    end = loop.time() + secs
    while not pred():
        assert loop.time() < end, "condition not reached"
        await asyncio.sleep(0.01)


def _calls_and_replies(wire):
    """The call/reply frames that crossed (auto-watch events and hellos aside)."""
    out = []
    for d, kind, f in wire:
        head = f[0] if kind == "binary" else f
        if head.get("type") in ("call", "reply"):
            out.append((d, kind, f))
    return out


async def test_c_a_ws_bridge_forward_text_call_binary_reply(two):
    ka, kb = two
    kb.create(
        "web_ws.tools",
        id="b_ws",
        ingress_rule="allow_all_ingress",
        can_call={"probe": "*"},
    )
    kb.create("anyverb_bytes_probe.tools", id="probe", callable_by={"b_ws": "*"})
    a = await _op(
        ka,
        "kernel",
        {
            "type": "create_agent",
            "parent_id": ka.id,
            "handler_module": "ws_bridge.tools",
            "transport": "memory",
            "peer_id": kb.id,
        },
    )
    a_leg = a["result"]["id"]
    mt_a, mt_b = MemoryTransport.pair()
    _test_transport_inject[a_leg] = mt_a
    sock = _WsEnd(mt_b)
    serving = asyncio.create_task(ws_run(sock, kb, kb.id, "b_ws"))
    try:
        assert (await _op(ka, a_leg, {"type": "boot"}))["result"].get("booted")
        out = await _op(
            ka,
            a_leg,
            {"type": "forward", "target": "probe", "payload": {"type": "events"}},
        )
        assert out["result"] == {"seq": 3, "chunk": CHUNK}, out
        crossed = _calls_and_replies(sock.wire)
        assert [(d, kind) for d, kind, _ in crossed] == [
            ("in", "text"),
            ("out", "binary"),
        ]
        call = crossed[0][2]
        assert call["target"] == "probe" and call["payload"] == {"type": "events"}
        head, body = crossed[1][2]
        assert head == {
            "type": "reply",
            "id": call["id"],
            "data": {"seq": 3, "chunk": None},
            "_binary_path": ["data", "chunk"],
        }
        assert body == CHUNK
    finally:
        await mt_a.close()
        await asyncio.wait_for(serving, 3)


async def test_d_a_peer_bridge_engine_answers_a_text_call_with_bytes(two):
    """H serves web_rest + web_ws; P dials H's web_ws with a ws_bridge and says
    `hello`. A JSON post to H's REST reaches `P-g|probe`: H's web_ws sends the
    call as TEXT, P's bridge ENGINE serves it and replies BINARY, H's REST
    answers octet."""
    kh, kp = two
    kh.create("web_ws.tools", id="h_ws", ingress_rule="allow_all_ingress")
    kh.create(
        "web_rest.tools",
        id="h_rest",
        ingress_rule="allow_all_ingress",
        can_call={"P-g|probe": "*"},
    )
    p = await _op(
        kp,
        "kernel",
        {
            "type": "create_agent",
            "parent_id": kp.id,
            "handler_module": "ws_bridge.tools",
            "transport": "memory",
            "peer_id": kh.id,
            "peer_guid": "A-g",
            "ingress_rule": "allow_all_ingress",
            "can_call": {"probe": "*"},
        },
    )
    kp.create("anyverb_bytes_probe.tools", id="probe", callable_by={"A-g|h_rest": "*"})
    mt_h, mt_p = MemoryTransport.pair()
    _test_transport_inject[p["result"]["id"]] = mt_p
    sock = _WsEnd(mt_h)
    serving = asyncio.create_task(ws_run(sock, kh, kh.id, "h_ws"))
    try:
        assert (await _op(kp, p["result"]["id"], {"type": "boot"}))["result"]["booted"]
        await mt_p.send({"type": "hello", "guid": "P-g"})
        await _until(lambda: "P-g" in kh.ctx.peers)
        app = FastAPI()
        app.add_api_route(
            "/h_rest/{target_id}",
            _make_post_endpoint("h_rest", kh),
            methods=["POST"],
        )
        async with httpx.AsyncClient(
            transport=httpx.ASGITransport(app=app), base_url="http://h"
        ) as c:
            r = await c.post("/h_rest/P-g|probe", json={"type": "events"})
        assert r.status_code == 200, r.text
        assert r.headers["content-type"] == "application/octet-stream"
        assert r.headers["X-Fantastic-Bytes-Field"] == "chunk"
        assert json.loads(r.headers["X-Fantastic-Payload"]) == {"seq": 3}
        assert r.content == CHUNK
        crossed = _calls_and_replies(sock.wire)
        assert [(d, kind) for d, kind, _ in crossed] == [
            ("out", "text"),
            ("in", "binary"),
        ]
        call = crossed[0][2]
        assert call["target"] == "probe" and call["payload"] == {"type": "events"}
        head, body = crossed[1][2]
        assert head == {
            "type": "reply",
            "id": call["id"],
            "data": {"seq": 3, "chunk": None},
            "_binary_path": ["data", "chunk"],
        }
        assert body == CHUNK
    finally:
        await mt_p.close()
        await asyncio.wait_for(serving, 3)
