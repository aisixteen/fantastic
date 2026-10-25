"""web_ws — twins of the Rust `fantastic-web` / `fantastic-web-ws` socket tests (#170).

Rust is the spec; each test is named `test_<rust_test>`.
"""

from __future__ import annotations

import asyncio
import json
import re
import struct
import types

from fastapi.testclient import TestClient

from kernel import register_unlisted
from web.app import make_app
from web_ws._proxy import _make_client_id, run
from web_ws.tools import _make_endpoint, handler

from _testkit import wire

HOST = "127.0.0.1:8088"


def _client(kernel, leg_id, *targets):
    """A web host's app with the ws endpoint of `leg_id` on it; the leg may call
    `kernel_state` and every name in `targets`."""
    kernel.get("test_web") or kernel.create(None, id="test_web", port=8088)
    app = make_app("test_web", kernel)
    app.add_api_websocket_route("/{host_id}/ws", _make_endpoint(leg_id, kernel))
    wire(kernel, leg_id, "kernel_state")
    for t in targets:
        wire(kernel, leg_id, t)
    return TestClient(app, headers={"host": HOST})


def _call(ws, **frame):
    ws.send_text(json.dumps({"type": "call", **frame}))


def _frames_until(ws, pred, n=60):
    """Every frame received (text parsed, binary kept raw) up to the first match."""
    out = []
    for _ in range(n):
        msg = ws.receive()
        if msg.get("text") is not None:
            f = json.loads(msg["text"])
        else:
            f = msg["bytes"]
        out.append(f)
        if pred(f):
            return out
    raise AssertionError(f"no matching frame in {n}: {out}")


def _reply_to(ws, fid):
    return _frames_until(ws, lambda f: isinstance(f, dict) and f.get("id") == fid)[-1]


def _binary_frame(head: dict, body: bytes) -> bytes:
    h = json.dumps(head).encode()
    return struct.pack(">I", len(h)) + h + body


def _head_of(wire_bytes: bytes) -> dict:
    n = struct.unpack(">I", wire_bytes[:4])[0]
    return json.loads(wire_bytes[4 : 4 + n])


def _make_leg(kernel, leg_id="leg"):
    kernel.create("web_ws.tools", id=leg_id, ingress_rule="allow_all_ingress")
    return leg_id


async def test_unknown_verb_is_unhandled_refusal(seeded_kernel):
    reply = await handler("ws", {"type": "shutdown"}, seeded_kernel)
    assert reply == {
        "error": "web_ws: unknown type 'shutdown'",
        "reason": "unhandled",
        "verb": "shutdown",
    }
    assert list(reply) == ["error", "reason", "verb"]


def test_ws_client_ids_are_pythons_token_hex_3_and_random():
    ids = [_make_client_id() for _ in range(64)]
    for cid in ids:
        assert re.fullmatch(r"_ws_[0-9a-f]{6}", cid), cid
    assert len(set(ids)) == len(ids), f"client ids repeated: {ids}"
    assert sorted(ids) != ids, f"client ids ascend like a counter: {ids}"


def test_call_frame_without_id_or_payload_replies_like_python(seeded_kernel):
    leg = _make_leg(seeded_kernel)
    with (
        _client(seeded_kernel, leg) as c,
        c.websocket_connect("/kernel_state/ws") as ws,
    ):
        # No target at all: `""`, which the kernel refuses — the URL host is
        # NEVER substituted. An absent id echoes as the empty string.
        ws.send_text(json.dumps({"type": "call"}))
        reply = _frames_until(
            ws, lambda f: isinstance(f, dict) and f.get("type") == "reply"
        )[-1]
        assert reply["id"] == "", reply
        assert reply["data"]["error"] == "send: target required", reply

        # A non-string id is echoed VERBATIM and still dispatches.
        _call(ws, target="kernel_state", id=7, payload={"type": "reflect"})
        reply = _reply_to(ws, 7)
        assert reply["type"] == "reply", reply
        assert isinstance(reply["data"], dict) and reply["data"]["id"] == "kernel_state"

        # A missing payload is `{}`: answered, never a dropped socket.
        _call(ws, target="kernel_state", id="np")
        reply = _reply_to(ws, "np")
        assert reply["type"] == "reply" and isinstance(reply["data"], dict), reply
        assert "id" not in reply["data"] or reply["data"]["id"] != "np"


def test_a_text_write_stream_keeps_the_clients_bytes_value(
    seeded_kernel, tmp_path, monkeypatch
):
    monkeypatch.chdir(tmp_path)
    files = tmp_path / "files"
    files.mkdir()
    leg = _make_leg(seeded_kernel)
    seeded_kernel.create(
        "file_bridge.tools",
        allow=["."],
        id="fb_txt",
        root="files",
        ingress_rule="allow_all_ingress",
        callable_by={leg: ["*"]},
    )
    with _client(seeded_kernel, leg, "fb_txt") as c:
        with c.websocket_connect("/kernel_state/ws") as ws:
            # A JSON STRING where raw bytes belong — refused, nothing written.
            _call(
                ws,
                target="fb_txt",
                id="s1",
                payload={
                    "type": "write_stream",
                    "path": "t.txt",
                    "offset": 0,
                    "bytes": "xyz",
                },
            )
            data = _reply_to(ws, "s1")["data"]
            assert data["error"] == "write_stream: bytes (one raw chunk) required", data
            assert data["reason"] == "missing_arg", data
            assert data["arg"] == "bytes (one raw chunk)", data
            assert not (files / "t.txt").exists(), "the refusal still wrote"

            # NO key at all: an empty chunk.
            _call(
                ws,
                target="fb_txt",
                id="s2",
                payload={"type": "write_stream", "path": "t2.txt", "offset": 0},
            )
            data = _reply_to(ws, "s2")["data"]
            assert data["written"] == 0 and data["size"] == 0, data


_capture = types.ModuleType("parity_capture.tools")


def _plain(v):
    """The payload with every bytes value spelled out (so the reply stays JSON)."""
    if isinstance(v, bytes):
        return {"bytes": v.decode()}
    if isinstance(v, dict):
        return {k: _plain(x) for k, x in v.items()}
    if isinstance(v, list):
        return [_plain(x) for x in v]
    return v


async def _capture_handler(id, payload, kernel):
    if payload.get("type") in ("boot", "shutdown"):
        return None
    return {"got": _plain(payload)}


_capture.handler = _capture_handler
_capture.VERBS = {}
register_unlisted("parity_capture.tools", _capture)


def test_a_body_reaches_its_handler_at_the_path_the_wire_named(seeded_kernel):
    leg = _make_leg(seeded_kernel)
    seeded_kernel.create("parity_capture.tools", id="cap", callable_by={leg: "*"})
    with _client(seeded_kernel, leg, "cap") as c:
        with c.websocket_connect("/kernel_state/ws") as ws:
            # A nested `_binary_path`: the body lands under `meta.blob`.
            head = {
                "type": "call",
                "id": "n1",
                "target": "cap",
                "payload": {"type": "take", "meta": {"k": 1, "blob": None}},
                "_binary_path": ["payload", "meta", "blob"],
            }
            ws.send_bytes(_binary_frame(head, b"abc"))
            got = _reply_to(ws, "n1")["data"]["got"]
            assert got == {"type": "take", "meta": {"k": 1, "blob": {"bytes": "abc"}}}

            # A LITERAL dotted key is one step: the body lands at the key `a.b`,
            # the nested twin untouched.
            head = {
                "type": "call",
                "id": "n2",
                "target": "cap",
                "payload": {"type": "take", "a": {"b": "kept"}, "a.b": None},
                "_binary_path": ["payload", "a.b"],
            }
            ws.send_bytes(_binary_frame(head, b"xy"))
            got = _reply_to(ws, "n2")["data"]["got"]
            assert got == {"type": "take", "a": {"b": "kept"}, "a.b": {"bytes": "xy"}}


# ─── a fake socket for the tests that must drive the kernel from the test itself ──


class _Sock:
    """The page's end of a socket: every frame the proxy SENDS lands in `out` as
    `("text", parsed)` or `("bytes", raw)`; `push*` is what the proxy RECEIVES."""

    def __init__(self):
        from starlette.websockets import WebSocketState

        self.application_state = WebSocketState.CONNECTED
        self.inbox: asyncio.Queue = asyncio.Queue()
        self.out: asyncio.Queue = asyncio.Queue()

    async def receive(self):
        return await self.inbox.get()

    async def send_text(self, text):
        await self.out.put(("text", json.loads(text)))

    async def send_bytes(self, data):
        await self.out.put(("bytes", data))

    def push(self, frame: dict):
        self.inbox.put_nowait({"type": "websocket.receive", "text": json.dumps(frame)})

    def push_bytes(self, data: bytes):
        self.inbox.put_nowait({"type": "websocket.receive", "bytes": data})

    def close(self):
        self.inbox.put_nowait({"type": "websocket.disconnect"})

    async def next(self, pred, timeout: float = 3.0):
        while True:
            kind, f = await asyncio.wait_for(self.out.get(), timeout)
            if pred(kind, f):
                return f


async def _open_sock(k, host_id, *, targets=()):
    k.create("web_ws.tools", id="leg", ingress_rule="allow_all_ingress")
    for t in (host_id, *targets):
        wire(k, "leg", t)
    ws = _Sock()
    task = asyncio.create_task(run(ws, k, host_id, "leg"))
    return ws, task


async def test_ws_watcher_gets_a_binary_frame_for_a_bytes_payload(seeded_kernel):
    k = seeded_kernel
    k.create("parity_capture.tools", id="watched")
    ws, task = await _open_sock(k, "watched")
    try:
        # the connect-time auto-watch of the host agent the socket named must land
        for _ in range(200):
            if k.ctx.agents["watched"]._watcher_ids:
                break
            await asyncio.sleep(0.01)
        assert k.ctx.agents["watched"]._watcher_ids, "the auto-watch never landed"
        blob = b"\xca\xfe\xba\xbe\x01"
        await k.send(
            [
                {
                    "target": "watched",
                    "payload": {"type": "write_stream", "path": "x.bin", "bytes": blob},
                }
            ]
        )
        wire_bytes = await ws.next(lambda kind, f: kind == "bytes")
        head = _head_of(wire_bytes)
        assert head["type"] == "event", head
        assert head["payload"]["type"] == "write_stream", head
        assert head["payload"]["path"] == "x.bin", head
        assert head["payload"]["bytes"] is None, head
        assert head["_binary_path"] == ["payload", "bytes"], head
        n = struct.unpack(">I", wire_bytes[:4])[0]
        assert wire_bytes[4 + n :] == blob, "the raw body rides beside the head"
    finally:
        ws.close()
        await asyncio.wait_for(task, 3)


async def test_the_watch_event_always_precedes_its_reply(seeded_kernel):
    k = seeded_kernel
    ws, task = await _open_sock(k, k.id)
    try:
        # The connect-time auto-watch mirrors the HOST's inbox, so a `call` at the
        # host is both the fanned event and the reply.
        for _ in range(200):
            if k.ctx.agents[k.id]._watcher_ids:
                break
            await asyncio.sleep(0.01)
        for n in range(60):
            fid = f"ord-{n}"
            ws.push(
                {
                    "type": "call",
                    "target": k.id,
                    "payload": {"type": "reflect"},
                    "id": fid,
                }
            )
            seen_event = False
            while True:
                kind, f = await asyncio.wait_for(ws.out.get(), 5)
                if kind != "text":
                    continue
                if f["type"] == "event" and f["payload"].get("type") == "reflect":
                    seen_event = True
                elif f["type"] == "reply" and f["id"] == fid:
                    assert seen_event, f"iteration {n}: the reply overtook the event"
                    break
    finally:
        ws.close()
        await asyncio.wait_for(task, 3)


async def test_a_raw_body_rides_the_call_both_ways(seeded_kernel):
    k = seeded_kernel
    k._meta["guid"] = "h0"
    ws, task = await _open_sock(k, k.id)
    try:
        ws.push({"type": "hello", "guid": "g1"})
        for _ in range(200):
            if "g1" in k.ctx.peers:
                break
            await asyncio.sleep(0.01)
        assert "g1" in k.ctx.peers
        send = asyncio.create_task(
            k.send(
                [
                    {
                        "target": "g1|files",
                        "payload": {
                            "type": "write_stream",
                            "path": "a.bin",
                            "bytes": b"ping",
                        },
                    }
                ]
            )
        )
        wire_bytes = await ws.next(lambda kind, f: kind == "bytes")
        head = _head_of(wire_bytes)
        assert head["type"] == "call", head
        assert head["target"] == "files", head
        assert head["_binary_path"] == ["payload", "bytes"], head
        assert head["payload"]["bytes"] is None, head
        n = struct.unpack(">I", wire_bytes[:4])[0]
        assert wire_bytes[4 + n :] == b"ping"
        ws.push_bytes(
            _binary_frame(
                {
                    "type": "reply",
                    "id": head["id"],
                    "data": {"ok": True, "bytes": None},
                    "_binary_path": ["data", "bytes"],
                },
                b"pong",
            )
        )
        out = (await asyncio.wait_for(send, 3))[0]
        assert out["result"] == {"ok": True, "bytes": b"pong"}, out
    finally:
        ws.close()
        await asyncio.wait_for(task, 3)


async def test_paste_image_reads_its_body_at_data_over_ws_and_rest(seeded_kernel):
    import sys

    from _testkit import child_env, command_rx
    from exec_bridge import tools as ebt
    from web_rest.tools import _make_post_endpoint

    k = seeded_kernel
    leg = _make_leg(k)
    k.create(
        "exec_bridge.tools",
        id="term",
        ingress_rule="allow_all_ingress",
        cwd=".",
        allowed=[rf"{command_rx(sys.executable)} -c .*"],
        inherit_env=child_env(),
    )
    k.create("web_rest.tools", id="rest_p", ingress_rule="allow_all_ingress")
    wire(k, "rest_p", "term")
    # a program that waits on its terminal
    started = (
        await k.send(
            [
                {
                    "target": "term",
                    "payload": {
                        "type": "start",
                        "command": sys.executable,
                        "args": ["-c", "import sys; sys.stdin.readline()"],
                        "pty": {"cols": 240, "rows": 24},
                    },
                }
            ]
        )
    )[0]["result"]
    assert started.get("pty") is True, started
    job = started["job_id"]
    png = b"\x89PNG\r\n\x1a\n-ws-"
    rest_png = b"\x89PNG\r\n\x1a\n-rest-"
    results: dict = {}

    def drive():
        with _client(k, leg, "term") as c:
            c.app.add_api_route(
                "/rest_p/{target_id}",
                _make_post_endpoint("rest_p", k),
                methods=["POST"],
            )
            with c.websocket_connect("/kernel_state/ws") as ws:
                # the body at `data`, from the WS binary frame's `_binary_path`
                head = {
                    "type": "call",
                    "id": "p1",
                    "target": "term",
                    "payload": {
                        "type": "paste_image",
                        "job_id": job,
                        "mime": "image/png",
                        "data": None,
                    },
                    "_binary_path": ["payload", "data"],
                }
                ws.send_bytes(_binary_frame(head, png))
                results["ws"] = _reply_to(ws, "p1")["data"]
                # the same body under `bytes` is not the image
                head["id"] = "p2"
                head["payload"] = {
                    "type": "paste_image",
                    "job_id": job,
                    "mime": "image/png",
                    "bytes": None,
                }
                head["_binary_path"] = ["payload", "bytes"]
                ws.send_bytes(_binary_frame(head, png))
                results["ws_wrong"] = _reply_to(ws, "p2")["data"]
            # REST: the octet body bound under the NAMED field
            r = c.post(
                "/rest_p/term",
                content=rest_png,
                headers={
                    "content-type": "application/octet-stream",
                    "X-Fantastic-Payload": json.dumps(
                        {"type": "paste_image", "job_id": job, "mime": "image/png"}
                    ),
                    "X-Fantastic-Bytes-Field": "data",
                },
            )
            results["rest_status"] = r.status_code
            results["rest"] = r.json()

    try:
        await asyncio.to_thread(drive)
        r = results["ws"]
        assert r["bytes"] == len(png), r
        assert r["path"].endswith(".png"), r
        with open(r["path"], "rb") as f:
            assert f.read() == png, "the body IS the image"
        w = results["ws_wrong"]
        assert (w["reason"], w["arg"]) == ("bad_arg", "data"), w
        assert results["rest_status"] == 200
        r = results["rest"]
        assert r["bytes"] == len(rest_png), r
        with open(r["path"], "rb") as f:
            assert f.read() == rest_png, "the REST body IS the image"
    finally:
        for j in list(ebt._jobs.values()):
            if j.get("owner") == "term":
                ebt._teardown(j)


def _free_port() -> int:
    import socket

    s = socket.socket()
    s.bind(("127.0.0.1", 0))
    port = s.getsockname()[1]
    s.close()
    return port


def _call_of_len(fid: str, n: int) -> str:
    """A `call` frame (reflect `kernel_state`) of EXACTLY `n` bytes, padded in the payload."""

    def frame(pad: str) -> str:
        return json.dumps(
            {
                "type": "call",
                "id": fid,
                "target": "kernel_state",
                "payload": {"type": "reflect", "tree": "none", "pad": pad},
            }
        )

    base = len(frame(""))
    out = frame("x" * (n - base))
    assert len(out.encode()) == n
    return out


async def _answer_or_end(conn, fid: str):
    """The reply to `fid`, or None when the socket ended without one."""
    import websockets

    while True:
        try:
            raw = await asyncio.wait_for(conn.recv(), 30)
        except websockets.ConnectionClosed:
            return None
        if isinstance(raw, str) and json.loads(raw).get("id") == fid:
            return json.loads(raw)


async def test_a_fragmented_message_is_bounded_by_the_one_frame_cap(seeded_kernel):
    import websockets

    from io_bridge import MAX_FRAME
    from web import tools as web_tools

    k = seeded_kernel
    port = _free_port()
    web_id = "web_msg_cap"
    rec = (
        await k.send(
            [
                {
                    "target": k.id,
                    "payload": {
                        "type": "create_agent",
                        "id": web_id,
                        "handler_module": "web.tools",
                        "parent_id": k.id,
                        "port": port,
                    },
                }
            ]
        )
    )[0]["result"]
    assert rec.get("id") == web_id, rec
    try:
        leg = (
            await k.send(
                [
                    {
                        "target": k.id,
                        "payload": {
                            "type": "create_agent",
                            "id": "cap_ws",
                            "handler_module": "web_ws.tools",
                            "parent_id": web_id,
                            "ingress_rule": "allow_all_ingress",
                        },
                    }
                ]
            )
        )[0]["result"]
        assert leg.get("id") == "cap_ws", leg
        wire(k, "cap_ws", "kernel_state")
        wire(k, web_id, "cap_ws", ["get_routes"])  # the host pulls the leg's routes
        mounted = (
            await k.send(
                [{"target": web_id, "payload": {"type": "mount", "child_id": "cap_ws"}}]
            )
        )[0]["result"]
        assert mounted == {"mounted": ["/{host_id}/ws"]}, mounted
        url = f"ws://127.0.0.1:{port}/{web_id}/ws"

        async def fragmented(fid: str, n: int):
            msg = _call_of_len(fid, n)
            half = len(msg) // 2
            async with websockets.connect(url, max_size=None) as conn:
                await conn.send([msg[:half], msg[half:]])  # two fragments
                return await _answer_or_end(conn, fid)

        # under the cap, in two fragments: reassembled and answered
        small = await fragmented("small", 4096)
        assert small is not None and small["type"] == "reply", small
        # one byte over the cap, every fragment well under it: the socket ends,
        # the call is never dispatched
        assert await fragmented("big", MAX_FRAME + 1) is None, "dispatched"
    finally:
        await k.send(
            [{"target": k.id, "payload": {"type": "delete_agent", "id": web_id}}]
        )
        web_tools._servers.pop(web_id, None)
