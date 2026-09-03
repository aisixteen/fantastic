"""web_ws — WS frame protocol (exercises web_ws._proxy via the web_ws surface)."""

from __future__ import annotations

import json
import struct

import pytest
from fastapi.testclient import TestClient

from io_bridge import decode_frame, encode_frame
from web.app import make_app
from web_ws.tools import _make_endpoint


@pytest.fixture
def client(seeded_kernel):
    app = make_app("test_web", seeded_kernel)
    # Sealed by default ⇒ open the leg so inbound frames dispatch (mirrors
    # test_gate.py::test_open_leg_dispatches).
    # Deny-all: opening the leg (ingress_rule) says WHO may call in; the surface's
    # can_call says WHERE those calls may land. `fb_ws` is the read_stream target.
    seeded_kernel.create(
        "web_ws.tools",
        id="test_web_ws",
        ingress_rule="allow_all_ingress",
        can_call={"fb_ws": "*"},
    )
    # Mount what web_ws.get_routes would publish — lets the proxy
    # tests use client.websocket_connect("/kernel_state/ws") without spinning
    # up a real `web` agent + child registration cycle.
    app.add_api_websocket_route(
        "/{host_id}/ws", _make_endpoint("test_web_ws", seeded_kernel)
    )
    with TestClient(app) as c:
        yield c


def _read_until_reply(ws, call_id, max_frames=10):
    """Drain frames from ws until the reply for `call_id` arrives. Auto-watch
    of the host agent may send `event` frames before the reply lands."""
    for _ in range(max_frames):
        msg = json.loads(ws.receive_text())
        if msg.get("type") == "reply" and msg.get("id") == call_id:
            return msg
    raise AssertionError(f"no reply for id={call_id} within {max_frames} frames")


def test_ws_call_returns_reply(client, seeded_kernel):
    """Send `reflect` to root via WS — the uniform identity + tree comes
    back (no primer keys; transports moved to the readme)."""
    with client.websocket_connect("/kernel_state/ws") as ws:
        ws.send_text(
            json.dumps(
                {
                    "type": "call",
                    "target": "kernel_state",
                    "payload": {"type": "reflect"},
                    "id": "1",
                }
            )
        )
        msg = _read_until_reply(ws, "1")
        assert msg["data"]["id"] == "kernel_state"
        assert msg["data"]["tree"]["id"] == "kernel_state"
        assert "transports" not in msg["data"]


def test_ws_emit_no_reply(client, seeded_kernel):
    with client.websocket_connect("/kernel_state/ws") as ws:
        ws.send_text(
            json.dumps(
                {
                    "type": "emit",
                    "target": "kernel_state",
                    "payload": {"type": "marker"},
                }
            )
        )
        # No reply expected; just verify connection still alive via a follow-up call
        ws.send_text(
            json.dumps(
                {
                    "type": "call",
                    "target": "kernel_state",
                    "payload": {"type": "reflect"},
                    "id": "x",
                }
            )
        )
        msg = _read_until_reply(ws, "x")
        assert msg["type"] == "reply"


def test_ws_watch_routes_events(client, seeded_kernel):
    with client.websocket_connect("/kernel_state/ws") as ws:
        ws.send_text(json.dumps({"type": "watch", "src": "kernel_state"}))
        ws.send_text(
            json.dumps(
                {
                    "type": "call",
                    "target": "kernel_state",
                    "payload": {"type": "reflect"},
                    "id": "1",
                }
            )
        )
        msg = _read_until_reply(ws, "1")
        assert msg["data"] is not None


def _send_frame(ws, frame):
    """Encode a frame via the shared codec and send it on the right WS frame type — a
    frame carrying raw `bytes` (a write_stream chunk) goes as a BINARY frame, a plain
    frame as TEXT. Mirrors what a real client (browser / WSTransport) does."""
    wire, is_binary = encode_frame(frame)
    if is_binary:
        ws.send_bytes(wire)
    else:
        ws.send_text(wire.decode("utf-8"))


def _recv_until_reply(ws, call_id, max_frames=12):
    """Drain frames (text OR binary) decoding each via the shared codec until the reply
    for `call_id` lands. A read_stream reply carries raw `bytes` ⇒ it arrives as a
    BINARY frame; auto-watch `event` frames may precede it."""
    for _ in range(max_frames):
        msg = ws.receive()
        raw = msg.get("text")
        if raw is None:
            raw = msg.get("bytes")
        f = decode_frame(raw)
        if f.get("type") == "reply" and f.get("id") == call_id:
            return f
    raise AssertionError(f"no reply for id={call_id} within {max_frames} frames")


def test_ws_read_stream_cursor_round_trips(
    client, seeded_kernel, tmp_path, monkeypatch
):
    """The stream CURSOR + RAW BYTES survive a REAL WS round-trip. The pointer is a
    plain payload field (offset out; next_offset/eof/size back); the chunk is raw
    `bytes` carried as a BINARY WS frame `[len|header|body]` — never base64. Push a
    multi-chunk binary file with `write_stream` (binary frame), pull it back over WS
    `call` frames threading `next_offset` (binary replies), reassemble byte-for-byte."""
    monkeypatch.chdir(tmp_path)
    # An OPEN file_bridge SOURCE (sealed-by-default ⇒ open it so the verbs dispatch).
    # Two-sided deny-all: reciprocally ACCEPT the ws leg that relays calls to it.
    seeded_kernel.create(
        "file_bridge.tools",
        id="fb_ws",
        root="sd",
        ingress_rule="allow_all_ingress",
        callable_by={"test_web_ws": "*"},
    )
    blob = bytes(range(256)) * 40  # 10240 bytes, binary

    with client.websocket_connect("/kernel_state/ws") as ws:
        # SINK over WS: push the whole blob as a BINARY frame (payload carries raw bytes).
        _send_frame(
            ws,
            {
                "type": "call",
                "id": "w",
                "target": "fb_ws",
                "payload": {
                    "type": "write_stream",
                    "path": "x.bin",
                    "bytes": blob,
                    "truncate": True,
                },
            },
        )
        assert _recv_until_reply(ws, "w")["data"]["size"] == len(blob)

        # SOURCE over WS: pull it back, threading the cursor frame-to-frame. The read
        # request has no bytes (text frame); each reply carries raw bytes (binary frame).
        got, offset, frames = b"", 0, 0
        while True:
            cid = f"r{frames}"
            ws.send_text(
                json.dumps(
                    {
                        "type": "call",
                        "id": cid,
                        "target": "fb_ws",
                        "payload": {
                            "type": "read_stream",
                            "path": "x.bin",
                            "offset": offset,
                            "length": 3000,
                        },
                    }
                )
            )
            data = _recv_until_reply(ws, cid)["data"]
            assert isinstance(data["bytes"], (bytes, bytearray))
            got += data["bytes"]
            offset = data["next_offset"]
            frames += 1
            if data["eof"]:
                break

    assert got == blob
    assert frames > 1, "multi-chunk: the cursor must have advanced across WS frames"
    assert data["size"] == len(blob)


def test_ws_call_state_event_carries_host_as_sender(client, seeded_kernel):
    """External WS `call` frames must tag state events with the
    web_ws surface's id so telemetry rays know where to start. Without
    this, browser-driven traffic looks "senderless" and the agent-vis
    never draws sender→recipient wires.
    """
    events: list[dict] = []
    unsub = seeded_kernel.add_state_subscriber(events.append)
    try:
        with client.websocket_connect("/kernel_state/ws") as ws:
            ws.send_text(
                json.dumps(
                    {
                        "type": "call",
                        "target": "kernel_state",
                        "payload": {"type": "reflect"},
                        "id": "1",
                    }
                )
            )
            _read_until_reply(ws, "1")
    finally:
        unsub()
    sends_to_core = [
        e
        for e in events
        if e.get("kind") == "send" and e.get("agent_id") == "kernel_state"
    ]
    assert sends_to_core, "no state event observed for the call"
    # Every external WS-driven send to kernel_state must originate from the
    # surface's id ("test_web_ws" in this fixture).
    senders = {e.get("sender") for e in sends_to_core}
    assert senders == {"test_web_ws"}, f"expected sender='test_web_ws', got {senders}"


# ─── malformed frames (a frame is an input, not a contract) ──────────


def _read_until_type(ws, ftype, max_frames=10):
    """Drain frames until one of `ftype` arrives (the connect-time auto-watch
    can put `event` frames ahead of it)."""
    for _ in range(max_frames):
        msg = json.loads(ws.receive_text())
        if msg.get("type") == ftype:
            return msg
    raise AssertionError(f"no {ftype!r} frame within {max_frames} frames")


@pytest.mark.parametrize(
    "send, raw",
    [
        ("bytes", b"\x00\x00\x00"),  # shorter than the 4-byte length prefix
        ("bytes", struct.pack(">I", 9999) + b"{}"),  # header length past the end
        ("bytes", struct.pack(">I", 8) + b"not json" + b"x"),  # bad header JSON
        ("text", "{not json"),  # unparseable text frame
        ("text", "[1]"),  # valid JSON, not an envelope
        ("text", "7"),  # ditto
    ],
)
def test_malformed_frame_answers_an_error_and_the_socket_survives(client, send, raw):
    """Before this, `decode_inbound`/`.get` raised out of the receive loop and the
    `finally` tore the whole connection down — cancelling every in-flight call on
    it (an LLM generation with them). Now the bad frame is answered and the next
    frame is served normally."""
    with client.websocket_connect("/kernel_state/ws") as ws:
        if send == "bytes":
            ws.send_bytes(raw)
        else:
            ws.send_text(raw)
        err = _read_until_type(ws, "error")
        assert err["reason"] == "bad_arg"
        assert "well-formed call/emit/watch envelope" in err["error"]
        # THE POINT: the socket is still usable.
        ws.send_text(
            json.dumps(
                {
                    "type": "call",
                    "target": "kernel_state",
                    "payload": {"type": "reflect", "tree": "none"},
                    "id": "after-bad",
                }
            )
        )
        msg = _read_until_reply(ws, "after-bad")
        assert msg["data"]["id"] == "kernel_state"
