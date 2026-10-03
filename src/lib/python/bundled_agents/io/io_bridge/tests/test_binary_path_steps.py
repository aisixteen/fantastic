"""`_binary_path` is a LIST OF STEPS (owner-approved, 2026-10-02).

A binary frame's header names where its raw body belongs. It used to spell that
as a dotted string, so a payload whose bytes sat at the LITERAL key `"a.b"` (which
web_rest's `X-Fantastic-Bytes-Field: a.b` binds) went out as `"payload.a.b"` and
the receiver rebuilt a NESTED `a` → `b` slot — over every bridge, and over the
exec_bridge guest wire. Now the path is a JSON array: a string step is an object
key, taken literally; an integer step is a list index.

  (a) the codec: literal, nested and list paths round-trip; every malformed path
      off the wire is one `ValueError`;
  (b) a ws_bridge forward through web_ws, both ways (call and reply);
  (c) a relay_connector forward through the (fake) relay;
  (d) web_rest's literal binding, then forwarded over a bridge to a peer kernel;
  (e) the exec_bridge guest wire: the frame a program writes, and a job's round
      trip through the kernel.
"""

from __future__ import annotations

import asyncio
import json
import socket
import struct
import subprocess
import sys
import types

import httpx
import pytest
from fastapi import FastAPI
from starlette.websockets import WebSocketState

from _testkit import boot_root
from _testkit_relay import _pair as relay_pair
from io_bridge import (
    ConnectionClosed,
    MemoryTransport,
    _bridges,
    _test_transport_inject,
    decode_frame,
    encode_frame,
)
from web_rest.tools import _make_post_endpoint
from web_ws._proxy import run as ws_run
from kernel import register_unlisted

# ─── the far kernel's probe ──────────────────────────────────────────────────

_got: list[dict] = []


async def _echo(id, payload, kernel):
    """Keeps what arrived; answers with bytes at the LITERAL key `x.y` beside a
    nested `x` → `y`, so the reply crosses the same way back."""
    verb = payload.get("type")
    if verb in ("boot", "shutdown", "reflect"):
        return {"id": id, "sentence": "binary path probe", "verbs": {}}
    _got.append(payload)
    return {
        "keys": sorted(payload),
        "x.y": payload.get("a.b", b""),
        "x": {"y": "kept"},
    }


_mod = types.ModuleType("binpath_echo.tools")
_mod.handler = _echo
_mod.VERBS = {}
register_unlisted("binpath_echo.tools", _mod)

SENT = {"type": "take", "a": {"b": "kept"}, "a.b": b"\x00raw\xff"}
BACK = {"keys": ["a", "a.b", "type"], "x.y": b"\x00raw\xff", "x": {"y": "kept"}}


def _head(wire: bytes) -> dict:
    (n,) = struct.unpack(">I", wire[:4])
    return json.loads(wire[4 : 4 + n])


def _bin(head: dict, body: bytes = b"zz") -> bytes:
    h = json.dumps(head).encode("utf-8")
    return struct.pack(">I", len(h)) + h + body


# ─── (a) the codec ───────────────────────────────────────────────────────────


def test_a_literal_dotted_key_round_trips_beside_its_nested_twin():
    env = {"type": "call", "id": "1", "target": "echo", "payload": dict(SENT)}
    wire, is_bin = encode_frame(env)
    assert is_bin is True
    head = _head(wire)
    assert head["_binary_path"] == ["payload", "a.b"]
    # the null sits at the literal key; the nested twin is untouched
    assert head["payload"] == {"type": "take", "a": {"b": "kept"}, "a.b": None}
    assert decode_frame(wire) == env


@pytest.mark.parametrize(
    "env, path",
    [
        ({"data": {"bytes": b"\x01"}}, ["data", "bytes"]),
        (
            {"data": {"items": [{"n": 1}, {"data": b"\x02"}]}},
            ["data", "items", 1, "data"],
        ),
        ({"payload": [b"\x03", "tail"]}, ["payload", 0]),
        # a key that looks like an index stays a string key
        ({"payload": {"0": b"\x04"}}, ["payload", "0"]),
        ({"payload": {"": b"\x05"}}, ["payload", ""]),
        ({"payload": {"a..b.": {"c.": b"\x06"}}}, ["payload", "a..b.", "c."]),
    ],
)
def test_nested_list_and_odd_key_paths_round_trip(env, path):
    wire, is_bin = encode_frame(env)
    assert is_bin and _head(wire)["_binary_path"] == path
    assert decode_frame(wire) == env


def test_a_path_with_no_slot_or_no_path_places_nothing():
    # absent or null ⇒ no body is placed, exactly as before
    assert decode_frame(_bin({"type": "x"})) == {"type": "x"}
    assert decode_frame(_bin({"type": "x", "_binary_path": None})) == {"type": "x"}


@pytest.mark.parametrize(
    "path, why",
    [
        ("payload.bytes", "not a list of steps"),  # the old dotted spelling
        ([], "an empty path names no slot"),
        ({"payload": "bytes"}, "not a list of steps"),
        ([0], "step 0 at an object is not a string key"),
        (["items", "0"], "step '0' at a list is not a non-negative integer index"),
        (["items", -1], "step -1 at a list is not a non-negative integer index"),
        (["items", True], "step True at a list is not a non-negative integer index"),
        (["items", 0.0], "step 0.0 at a list is not a non-negative integer index"),
        (["items", 2], "index 2 is past the end of a list of 2"),
        (["nope", "x"], "no key 'nope'"),
        (["type", "x"], "step 'x' goes into a string, not a container"),
        (["items", 0, "x"], "step 'x' goes into a null, not a container"),
        (["n", "x"], "step 'x' goes into a number, not a container"),
    ],
)
def test_a_malformed_path_off_the_wire_is_one_value_error(path, why):
    head = {"type": "reply", "items": [None, None], "n": 1, "_binary_path": path}
    with pytest.raises(ValueError) as e:
        decode_frame(_bin(head))
    assert str(e.value) == f"bad _binary_path {path!r}: {why}"


def test_bytes_under_a_non_string_key_are_refused_at_the_sender():
    with pytest.raises(TypeError, match="non-string key 1"):
        encode_frame({"type": "reply", "data": {1: b"x"}})


# ─── two kernels ─────────────────────────────────────────────────────────────


async def _op(k, target, payload, **item):
    return (await k.send([{"target": target, "payload": payload, **item}]))[0]


def _kernel(path, monkeypatch, guid: str):
    path.mkdir()
    monkeypatch.chdir(path)
    k = boot_root()
    k.update(k.id, guid=guid)
    return k


@pytest.fixture
async def two(tmp_path, monkeypatch):
    _got.clear()
    ka = _kernel(tmp_path / "a", monkeypatch, "A-g")
    kb = _kernel(tmp_path / "b", monkeypatch, "B-g")
    monkeypatch.chdir(tmp_path)
    yield ka, kb
    _bridges.clear()
    _test_transport_inject.clear()


class _WsEnd:
    """A web_ws socket whose far end is a bridge leg's MemoryTransport half —
    every frame crosses the REAL codec. `heads` keeps the binary heads that
    crossed, both ways (the test's capture)."""

    def __init__(self, mt: MemoryTransport):
        self.application_state = WebSocketState.CONNECTED
        self.mt = mt
        self.heads: list[dict] = []

    async def receive(self):
        try:
            frame = await self.mt.recv()
        except ConnectionClosed:
            return {"type": "websocket.disconnect"}
        raw, is_bin = encode_frame(frame)
        if is_bin:
            self.heads.append(_head(raw))
            return {"type": "websocket.receive", "bytes": raw}
        return {"type": "websocket.receive", "text": raw.decode("utf-8")}

    async def _out(self, raw):
        if isinstance(raw, bytes):
            self.heads.append(_head(raw))
        await self.mt.send(decode_frame(raw))

    async def send_text(self, text):
        await self._out(text)

    async def send_bytes(self, data):
        await self._out(data)


async def _until(pred, secs=3.0):
    loop = asyncio.get_running_loop()
    end = loop.time() + secs
    while not pred():
        assert loop.time() < end, "condition not reached"
        await asyncio.sleep(0.01)


# ─── (b) a ws_bridge forward through web_ws ──────────────────────────────────


async def test_a_literal_key_crosses_a_ws_bridge_forward_both_ways(two):
    ka, kb = two
    kb.create(
        "web_ws.tools",
        id="b_ws",
        ingress_rule="allow_all_ingress",
        can_call={"echo": "*"},
    )
    kb.create("binpath_echo.tools", id="echo", callable_by={"b_ws": "*"})
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
            {"type": "forward", "target": "echo", "payload": dict(SENT)},
        )
        assert _got == [SENT], _got
        assert out["result"] == BACK, out
        assert [h["_binary_path"] for h in sock.heads] == [
            ["payload", "a.b"],
            ["data", "x.y"],
        ]
    finally:
        await mt_a.close()
        await asyncio.wait_for(serving, 3)


# ─── (c) a relay_connector forward through the relay ─────────────────────────


async def test_a_literal_key_crosses_a_relay_forward_both_ways(two):
    ka, kb = two
    kb.create("binpath_echo.tools", id="echo", callable_by={"A-g|*": "*"})
    a = await _op(
        ka,
        "kernel",
        {
            "type": "create_agent",
            "parent_id": ka.id,
            "handler_module": "relay_connector.tools",
            "transport": "memory",
            "relay_url": "ws://relay",
            "guid": "RA",
            "partner_guid": "RB",
        },
    )
    b = await _op(
        kb,
        "kernel",
        {
            "type": "create_agent",
            "parent_id": kb.id,
            "handler_module": "relay_connector.tools",
            "transport": "memory",
            "relay_url": "ws://relay",
            "guid": "RB",
            "partner_guid": "RA",
            "peer_guid": "A-g",
            "ingress_rule": "allow_all_ingress",
            "can_call": {"echo": "*"},
        },
    )
    a_id, b_id = a["result"]["id"], b["result"]["id"]
    ta, tb = relay_pair("RA", "RB")
    _test_transport_inject[a_id] = ta
    _test_transport_inject[b_id] = tb
    for k, i in ((ka, a_id), (kb, b_id)):
        assert (await _op(k, i, {"type": "boot"}))["result"].get("booted") is True
    out = await _op(
        ka, a_id, {"type": "forward", "target": "echo", "payload": dict(SENT)}
    )
    assert _got == [SENT], _got
    assert out["result"] == BACK, out


# ─── (d) web_rest's literal binding, forwarded to a peer kernel ──────────────


async def test_web_rest_literal_binding_stays_literal_across_a_bridge(two):
    """H serves web_rest + web_ws; P dials H's web_ws with a ws_bridge and says
    `hello`, so H reaches P's agents as `P-g|<agent>`. A REST post binds the body
    to the literal key `a.b`; H forwards it to `P-g|echo` over the socket; the
    reply's bytes at `x.y` come back the same way and out as the octet body."""
    kh, kp = two
    kh.create("web_ws.tools", id="h_ws", ingress_rule="allow_all_ingress")
    kh.create(
        "web_rest.tools",
        id="h_rest",
        ingress_rule="allow_all_ingress",
        can_call={"P-g|echo": "*"},
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
            "can_call": {"echo": "*"},
        },
    )
    kp.create("binpath_echo.tools", id="echo", callable_by={"A-g|h_rest": "*"})
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
            r = await c.post(
                "/h_rest/P-g|echo",
                content=b"\x00raw\xff",
                headers={
                    "content-type": "application/octet-stream",
                    "X-Fantastic-Payload": json.dumps(
                        {"type": "take", "a": {"b": "kept"}}
                    ),
                    "X-Fantastic-Bytes-Field": "a.b",
                },
            )
        assert r.status_code == 200, r.text
        assert _got == [SENT], _got
        assert r.content == b"\x00raw\xff"
        assert r.headers["X-Fantastic-Bytes-Field"] == "x.y"
        assert json.loads(r.headers["X-Fantastic-Payload"]) == {
            "keys": ["a", "a.b", "type"],
            "x": {"y": "kept"},
        }
        assert [h["_binary_path"] for h in sock.heads] == [
            ["payload", "a.b"],
            ["data", "x.y"],
        ]
    finally:
        await mt_p.close()
        await asyncio.wait_for(serving, 3)


# ─── (e) the exec_bridge guest wire ──────────────────────────────────────────


def _recv_frame(sock: socket.socket) -> bytes:
    def exact(n):
        b = b""
        while len(b) < n:
            c = sock.recv(n - len(b))
            assert c, "the program closed the fd"
            b += c
        return b

    (n,) = struct.unpack(">I", exact(4))
    return exact(n)


def test_the_guest_wire_carries_a_literal_key_both_ways():
    """The frame a program writes names the literal key; a kind-B reply placed by
    the kernel's own codec lands at the literal key in the program."""
    from exec_bridge._connector import CONNECTOR_SRC

    code = CONNECTOR_SRC + (
        "r = kernel.send('echo', {'type': 'take', 'a': {'b': 'kept'}, "
        "'a.b': b'\\x00raw\\xff', 'items': [1]})\n"
        "print(sorted(r), r['x.y'], r['x'])\n"
    )
    parent, child_end = socket.socketpair()
    proc = subprocess.Popen(
        [sys.executable, "-u", "-c", code],
        env={"FANTASTIC_CTRL_FD": str(child_end.fileno()), "PATH": "/usr/bin:/bin"},
        pass_fds=[child_end.fileno()],
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
    )
    child_end.close()
    try:
        parent.settimeout(10)
        body = _recv_frame(parent)
        assert body[:1] == b"B", body[:1]
        head = _head(body[1:])
        assert head["_binary_path"] == ["payload", "a.b"]
        assert head["payload"] == {
            "type": "take",
            "a": {"b": "kept"},
            "a.b": None,
            "items": [1],
        }
        assert body[5 + struct.unpack(">I", body[1:5])[0] :] == b"\x00raw\xff"
        wire, is_bin = encode_frame(
            {
                "op": "reply",
                "rid": head["rid"],
                "data": {"x.y": b"back", "x": {"y": "kept"}},
            }
        )
        assert is_bin and _head(wire)["_binary_path"] == ["data", "x.y"]
        data = b"B" + wire
        parent.sendall(struct.pack(">I", len(data)) + data)
        out, err = proc.communicate(timeout=10)
    finally:
        parent.close()
        if proc.poll() is None:
            proc.kill()
    assert out.decode().strip() == "['x', 'x.y'] b'back' {'y': 'kept'}", (out, err)


def test_the_guest_wire_carries_a_list_index_from_the_program():
    from exec_bridge._connector import CONNECTOR_SRC

    code = CONNECTOR_SRC + (
        "kernel.emit({'type': 'chunks', 'items': [{'n': 0}, {'data': b'hi'}]})\n"
    )
    parent, child_end = socket.socketpair()
    proc = subprocess.Popen(
        [sys.executable, "-u", "-c", code],
        env={"FANTASTIC_CTRL_FD": str(child_end.fileno()), "PATH": "/usr/bin:/bin"},
        pass_fds=[child_end.fileno()],
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
    )
    child_end.close()
    try:
        parent.settimeout(10)
        body = _recv_frame(parent)
        assert body[:1] == b"B"
        assert _head(body[1:])["_binary_path"] == ["payload", "items", 1, "data"]
        assert decode_frame(body[1:]) == {
            "op": "emit",
            "payload": {"type": "chunks", "items": [{"n": 0}, {"data": b"hi"}]},
        }
        proc.communicate(timeout=10)
    finally:
        parent.close()
        if proc.poll() is None:
            proc.kill()


async def test_a_job_round_trips_a_literal_key_through_the_kernel(
    seeded_kernel, tmp_path, monkeypatch
):
    """The whole guest path: program → relay (`decode_frame`) → echo → relay
    (`encode_frame`) → program."""
    from exec_bridge._connector import CONNECTOR_SRC

    monkeypatch.chdir(tmp_path)
    _got.clear()
    k = seeded_kernel
    py = sys.executable
    k.create("binpath_echo.tools", id="echo", callable_by={"runner": "*"})
    made = await _op(
        k,
        k.id,
        {
            "type": "create_agent",
            "parent_id": k.id,
            "id": "runner",
            "handler_module": "exec_bridge.tools",
            "ingress_rule": "allow_all_ingress",
            "cwd": ".",
            "inherit_env": ["PATH"],
            "allowed": [rf"{py} -c .*"],
            "can_call": {"echo": "*"},
        },
    )
    assert made["result"].get("id") == "runner", made
    child = (
        "r = kernel.send('echo', {'type': 'take', 'a': {'b': 'kept'}, "
        "'a.b': b'\\x00raw\\xff'})\n"
        "print('GOT', r['keys'], r['x.y'] == b'\\x00raw\\xff', r['x'])\n"
    )
    started = await _op(
        k,
        "runner",
        {"type": "start", "command": py, "args": ["-c", CONNECTOR_SRC + child]},
    )
    job_id = started["result"]["job_id"]
    for _ in range(300):
        st = await _op(k, "runner", {"type": "status", "job_id": job_id})
        row = st["result"]["jobs"][0]
        if row["status"] != "running":
            break
        await asyncio.sleep(0.05)
    assert row["exit_code"] == 0, row
    assert row["last_line"] == "GOT ['a', 'a.b', 'type'] True {'y': 'kept'}", row
    assert _got == [SENT], _got
