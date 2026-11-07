"""THE TWO-WAY KERNEL BRIDGE (issue #476): kernel A's `ws_bridge` dials kernel B's
`web_ws` and says `hello {guid, leg}`; from then on B calls A's agents as
`<A guid>|<agent>` and watches them, gated by A's leg (its `ingress_rule`, its
`can_call`, the target's `callable_by`). The twin of rust
`fantastic-web/src/tests_two_way.rs`.

Every case runs on BOTH transports: `memory` (A's leg on an in-process half whose
far end is a socket into B's proxy loop) and `ws` (A's leg dials with its real
`ws` transport — `WSTransport` over a websockets client the test connects to
B's proxy loop, so the test can drop the link under it).
"""

from __future__ import annotations

import asyncio
import itertools
import json

import pytest
import websockets
from starlette.websockets import WebSocketState
from websockets.protocol import State

from _testkit import boot_root, memory_meta
from io_bridge import (
    ConnectionClosed,
    MemoryTransport,
    _bridges,
    _test_transport_inject,
    decode_frame,
    encode_frame,
)
from kernel import register_unlisted
from web_ws._proxy import run as ws_run

import types

A = "A-g"
VALUE = "sk-A-STORE-41f7c2"
WIRES = ["memory", "ws"]
# an AI's turn state is process-wide, keyed by id: every pair names its AI apart
_AI_SEQ = itertools.count()


# ─── the probe on A ──────────────────────────────────────────────────────────

_held: list[asyncio.Event] = []


async def _probe(id, payload, kernel):
    """`echo` answers `{echo: text}`; `hold` never answers (a call in flight when
    the link drops) and says so on `_held`."""
    verb = payload.get("type")
    if verb in ("boot", "shutdown", "reflect"):
        return {"id": id, "sentence": "probe", "verbs": {}}
    if verb == "echo":
        return {"echo": payload.get("text")}
    if verb == "hold":
        _held[0].set()
        await asyncio.Event().wait()
    return None


_mod = types.ModuleType("two_way_probe.tools")
_mod.handler = _probe
_mod.VERBS = {}
register_unlisted("two_way_probe.tools", _mod)


class _Canned:
    """A scripted model: one text part, then `done`, a few characters a token."""

    async def chat(self, messages, max_tokens=None):
        for t in ["Hel", "lo th", "ere ", '<call name="done"></call>']:
            yield t


# ─── the sockets ─────────────────────────────────────────────────────────────


class _MemEnd:
    """B's end of a socket whose far end is A's leg's MemoryTransport half —
    every frame crosses the real codec."""

    def __init__(self, mt: MemoryTransport, peer: MemoryTransport):
        self.application_state = WebSocketState.CONNECTED
        self.mt, self.peer = mt, peer

    async def receive(self):
        try:
            frame = await self.mt.recv()
        except ConnectionClosed:
            return {"type": "websocket.disconnect"}
        raw, is_bin = encode_frame(frame)
        if is_bin:
            return {"type": "websocket.receive", "bytes": raw}
        return {"type": "websocket.receive", "text": raw.decode("utf-8")}

    async def send_text(self, text):
        await self._out(text)

    async def send_bytes(self, data):
        await self._out(data)

    async def _out(self, raw):
        try:
            await self.mt.send(decode_frame(raw))
        except ConnectionClosed:
            pass

    async def close(self, code=1000, reason=None):
        await self.cut()

    async def cut(self):
        self.application_state = WebSocketState.DISCONNECTED
        await self.mt.close()
        await self.peer.close()


class _WsLink:
    """One socket: the server half B's proxy drives, and a websockets-like client
    half A's `WSTransport` reads and writes."""

    _GONE = object()

    def __init__(self):
        self.application_state = WebSocketState.CONNECTED
        self.state = State.OPEN  # the client's view
        self._up: asyncio.Queue = asyncio.Queue()
        self._down: asyncio.Queue = asyncio.Queue()

    # the server half (starlette's WebSocket, as `run` uses it)
    async def receive(self):
        msg = await self._up.get()
        if msg is self._GONE:
            return {"type": "websocket.disconnect"}
        return msg

    async def send_text(self, text):
        if self.state is State.OPEN:
            self._down.put_nowait(text)

    async def send_bytes(self, data):
        if self.state is State.OPEN:
            self._down.put_nowait(data)

    async def close(self, code=1000, reason=None):
        await self.cut()

    # the client half (a `websockets` connection, as `WSTransport` uses it)
    async def send(self, data):
        if self.state is not State.OPEN:
            raise websockets.ConnectionClosedError(None, None)
        key = "bytes" if isinstance(data, bytes) else "text"
        self._up.put_nowait({"type": "websocket.receive", key: data})

    async def recv(self):
        msg = await self._down.get()
        if msg is self._GONE:
            raise websockets.ConnectionClosedError(None, None)
        return msg

    async def cut(self):
        if self.state is State.CLOSED:
            return
        self.state = State.CLOSED
        self.application_state = WebSocketState.DISCONNECTED
        self._up.put_nowait(self._GONE)
        self._down.put_nowait(self._GONE)


class _Raw:
    """A test-held socket into B's proxy: frames it pushes are what the proxy
    receives; what the proxy sends lands in `out`."""

    def __init__(self):
        self.application_state = WebSocketState.CONNECTED
        self.inbox: asyncio.Queue = asyncio.Queue()
        self.out: asyncio.Queue = asyncio.Queue()

    async def receive(self):
        return await self.inbox.get()

    async def send_text(self, text):
        await self.out.put(json.loads(text))

    async def send_bytes(self, data):
        raise AssertionError("unexpected binary frame")

    async def close(self, code=1000, reason=None):
        self.application_state = WebSocketState.DISCONNECTED
        self.inbox.put_nowait({"type": "websocket.disconnect"})

    def push(self, frame: dict):
        self.inbox.put_nowait({"type": "websocket.receive", "text": json.dumps(frame)})

    async def next(self, ftype: str) -> dict:
        while True:
            f = await asyncio.wait_for(self.out.get(), 3)
            if f.get("type") == ftype:
                return f


# ─── the two kernels ─────────────────────────────────────────────────────────


async def _op(k, target, payload, **item):
    return (await k.send([{"target": target, "payload": payload, **item}]))[0]


async def _until(pred, secs=3.0):
    loop = asyncio.get_running_loop()
    end = loop.time() + secs
    while not pred():
        assert loop.time() < end, "condition not reached"
        await asyncio.sleep(0.01)


class _Pair:
    """Kernel A with its leg dialed into kernel B's `web_ws`."""

    def __init__(self, wire, ka, kb, leg, ai, monkeypatch):
        self.wire, self.ka, self.kb, self.leg, self.ai = wire, ka, kb, leg, ai
        self.serving: list[asyncio.Task] = []
        self.link = None
        if wire == "ws":

            async def connect(url, **kw):
                assert url == f"ws://127.0.0.1:8088/{kb.id}/ws", url
                link = _WsLink()
                self._serve(link)
                self.link = link
                return link

            monkeypatch.setattr(websockets, "connect", connect)

    def _serve(self, sock):
        self.serving.append(
            asyncio.create_task(ws_run(sock, self.kb, self.kb.id, "b_ws"))
        )

    async def dial(self):
        """Boot A's leg (the first dial, or a redial after a drop): it says hello,
        and B routes A's guid through it."""
        if self.wire == "memory":
            mt_a, mt_b = MemoryTransport.pair()
            _test_transport_inject[self.leg] = mt_a
            self.link = _MemEnd(mt_b, mt_a)
            self._serve(self.link)
        booted = (await _op(self.ka, self.leg, {"type": "boot"}))["result"]
        assert booted.get("booted") is True, (self.wire, booted)
        await _until(lambda: len(self.kb.ctx.peers.get(A, ())) == 1)

    async def cut(self):
        """Drop the link under A's leg, then wait until both ends know."""
        await self.link.cut()
        await _until(lambda: A not in self.kb.ctx.peers)
        await _until(lambda: not self.reflect_connected())

    def reflect_connected(self) -> bool:
        st = _bridges.get(self.leg)
        return st is not None and st.transport is not None and not st.transport.closed

    async def call(self, agent, payload, **item):
        return await _op(self.kb, f"{A}|{agent}", payload, **item)

    def patch_leg(self, **meta):
        self.ka.update(self.leg, **meta)

    async def until_watched(self, agent, on):
        await _until(lambda: self.ka.has_watchers(agent) == on)

    async def stop(self):
        await _op(self.ka, "kernel", {"type": "delete_agent", "id": self.leg})
        if self.link is not None:
            await self.link.cut()
        for t in self.serving:
            await asyncio.wait_for(t, 3)


@pytest.fixture
def pair_of(tmp_path, monkeypatch, fake_providers):
    made: list[_Pair] = []

    async def make(wire: str, *, open_: bool = True, granted: bool = True) -> _Pair:
        n = len(made)
        b_dir, a_dir = tmp_path / f"b{n}", tmp_path / f"a{n}"
        b_dir.mkdir()
        a_dir.mkdir()
        monkeypatch.chdir(b_dir)
        kb = boot_root()
        kb.update(kb.id, guid="B-g")
        kb.create(
            "web_ws.tools",
            id="b_ws",
            ingress_rule="allow_all_ingress",
            can_call={"probe": "*"},
        )
        kb.create("two_way_probe.tools", id="probe", callable_by={"b_ws": "*"})
        monkeypatch.chdir(a_dir)
        ka = boot_root()
        ka.update(ka.id, guid=A)
        leg, ai = "a_leg", f"ai{next(_AI_SEQ)}"
        accept = {leg: "*"} if granted else {}
        ka.create("two_way_probe.tools", id="probe", callable_by=dict(accept))
        ka.create(
            "ollama_backend.tools",
            id=ai,
            context_window=32768,
            **memory_meta(ka, ai, {"cli": "*"}),
            callable_by=dict(accept),
        )
        fake_providers[ai] = _Canned()
        ka.create(
            "credential_store.tools",
            id="creds",
            file=str(a_dir / "creds.yaml"),
            callable_by=dict(accept),
        )
        await _op(
            ka,
            "kernel",
            {
                "type": "update_agent",
                "id": "creds",
                "refs": {"near/main": {"host": "api.example", "fields": ["api_key"]}},
            },
        )
        rec = {"id": leg, "peer_id": kb.id}
        if wire == "memory":
            rec["transport"] = "memory"
        else:
            rec.update(transport="ws", host="127.0.0.1", port=8088)
        if open_:
            rec["ingress_rule"] = "allow_all_ingress"
        if granted:
            rec["can_call"] = {"probe": "*", ai: "*", "creds": "*"}
        made_rec = ka.create("ws_bridge.tools", **rec)
        assert made_rec.get("id") == leg, made_rec
        monkeypatch.chdir(tmp_path)
        p = _Pair(wire, ka, kb, leg, ai, monkeypatch)
        await p.dial()
        made.append(p)
        return p

    _held.clear()
    _held.append(asyncio.Event())
    yield make
    _bridges.clear()
    _test_transport_inject.clear()


def _turn(text: str) -> dict:
    """A turn on A's AI for a non-cli caller — its stream goes to the AI's inbox,
    so its watchers see it."""
    return {"type": "send", "text": text, "client_id": "remote"}


def _drain(q: asyncio.Queue) -> list[dict]:
    out = []
    while not q.empty():
        out.append(q.get_nowait())
    return out


def _kinds(evs: list[dict]) -> list[str]:
    return [
        f"status:{e.get('phase')}" if e.get("type") == "status" else e.get("type")
        for e in evs
    ]


def _assert_stream_before_done(evs: list[dict], wire: str) -> None:
    """A full turn's stream as a watcher sees it: status phases, the text part
    streamed, all before `done`."""
    k = _kinds(evs)
    assert "status:done" in k, (wire, k)
    before = k[: k.index("status:done")]
    assert any(t.startswith("status:") for t in before), (wire, k)
    for part in ("part_start", "part_delta", "part_end"):
        assert part in before, (wire, part, k)


# ─── hello ───────────────────────────────────────────────────────────────────


@pytest.mark.parametrize("wire", WIRES)
async def test_hello_makes_a_kernel_leg_and_a_plain_client_stays_a_client(
    pair_of, wire
):
    p = await pair_of(wire)
    try:
        assert p.kb.ctx.peer_rows() == [{"guid": A, "via": "b_ws"}]
        # a connection that says no hello is the canvas page's: it calls, it is
        # answered, it is no address
        page = _Raw()
        serving = asyncio.create_task(ws_run(page, p.kb, p.kb.id, "b_ws"))
        page.push(
            {
                "type": "call",
                "id": "c1",
                "target": "probe",
                "payload": {"type": "echo", "text": "page"},
            }
        )
        assert (await page.next("reply"))["data"] == {"echo": "page"}
        assert len(p.kb.ctx.peer_rows()) == 1
        await page.close()
        await asyncio.wait_for(serving, 3)
        # and B reaches A's agent through the leg
        out = await p.call("probe", {"type": "echo", "text": "hi"})
        assert out["result"] == {"echo": "hi"}, out
    finally:
        await p.stop()


# ─── calls B → A, gated as A's leg ───────────────────────────────────────────


@pytest.mark.parametrize("wire", WIRES)
async def test_a_call_from_b_is_gated_by_the_legs_seal_and_grants(pair_of, wire):
    p = await pair_of(wire, open_=False, granted=False)
    echo = {"type": "echo", "text": "x"}
    try:
        # sealed by default: the teaching denial
        sealed = (await p.call("probe", echo))["result"]
        assert sealed["reason"] == "unauthorized" and "hint" in sealed, sealed
        # opened, but no grant: the leg may not call the probe
        p.patch_leg(ingress_rule="allow_all_ingress")
        ungranted = (await p.call("probe", echo))["result"]
        assert ungranted["reason"] == "unauthorized", ungranted
        assert ungranted["side"] == "egress", ungranted
        # granted both ends: it runs
        p.patch_leg(can_call={"probe": "*"})
        p.ka.update("probe", callable_by={p.leg: "*"})
        assert (await p.call("probe", echo))["result"] == {"echo": "x"}
        # sealed again: refused again, live
        p.patch_leg(ingress_rule="deny_ingress")
        resealed = (await p.call("probe", echo))["result"]
        assert resealed["reason"] == "unauthorized", resealed
    finally:
        await p.stop()


# ─── watches B → A, events A → B ─────────────────────────────────────────────


@pytest.mark.parametrize("wire", WIRES)
async def test_b_watches_an_ai_on_a_and_gets_its_stream_until_unwatch_or_drop(
    pair_of, wire
):
    p = await pair_of(wire)
    target = f"{A}|{p.ai}"
    try:
        q = p.kb.ctx.ensure_inbox("watcher")
        assert p.kb.watch(target, "watcher") is None
        await p.until_watched(p.ai, True)

        out = await p.call(p.ai, _turn("one"))
        assert out["status"] == "completed", out
        assert out["result"]["client_id"] == "remote", out
        _assert_stream_before_done(_drain(q), wire)

        # unwatched: A stops streaming, the next turn brings nothing
        p.kb.unwatch(target, "watcher")
        await p.until_watched(p.ai, False)
        assert (await p.call(p.ai, _turn("two")))["status"] == "completed"
        assert _drain(q) == []

        # watched again, then the link drops: A gives the watch back
        assert p.kb.watch(target, "watcher") is None
        await p.until_watched(p.ai, True)
        await p.cut()
        await p.until_watched(p.ai, False)
        assert (await _op(p.ka, p.ai, _turn("three")))["status"] == "completed"
        assert _drain(q) == []
    finally:
        await p.stop()


@pytest.mark.parametrize("wire", WIRES)
async def test_a_watch_from_b_is_gated_by_the_legs_watch_grant(pair_of, wire):
    p = await pair_of(wire, granted=False)
    try:
        q = p.kb.ctx.ensure_inbox("watcher")
        # B's own side takes it (B's operator); A's leg holds no `watch` on `ai`
        assert p.kb.watch(f"{A}|{p.ai}", "watcher") is None
        # A's leg refuses it: the refusal reaches the watcher, named, and B
        # forgets the watch it never got
        refusal = await asyncio.wait_for(q.get(), 3.0)
        assert refusal["type"] == "error", refusal
        assert refusal["src"] == p.ai, refusal
        assert refusal["reason"] == "unauthorized", refusal
        assert "id" not in refusal, refusal
        assert p.kb.ctx.peer_watched(A) == []
        # the leg serves calls but not watches: the turn runs, nothing streams
        p.patch_leg(can_call={p.ai: ["send"]})
        p.ka.update(p.ai, callable_by={p.leg: "*"})
        out = await p.call(p.ai, _turn("one"))
        assert out["status"] == "completed", out
        assert not p.ka.has_watchers(p.ai)
        assert _drain(q) == []
    finally:
        await p.stop()


# ─── secret marks ────────────────────────────────────────────────────────────


@pytest.mark.parametrize("wire", WIRES)
async def test_a_reply_secret_from_a_is_secret_in_bs_records(pair_of, wire):
    p = await pair_of(wire)
    try:
        await _op(
            p.ka,
            "creds",
            {"type": "set", "ref": "near/main", "fields": {"api_key": VALUE}},
        )
        events: list[dict] = []
        off = p.kb.ctx.add_state_subscriber(events.append)
        try:
            out = await p.call(
                "creds", {"type": "get", "ref": "near/main"}, id="b-get-1"
            )
        finally:
            off()
        # the caller gets it whole, the outcome names the marks
        assert out["result"]["fields"]["api_key"] == VALUE, out
        assert out["secret"] == ["fields.api_key"], out
        # B's settle row (named: it keeps the result) shows [secret]
        settles = [
            r
            for r in p.kb.ctx.rows.values()
            if r.get("kind") == "settle" and r.get("parent") == out["pid"]
        ]
        assert settles and settles[0]["outcome"]["result"]["fields"] == {
            "api_key": "[secret]"
        }, settles
        assert VALUE not in repr(events), "B's state stream holds the value"
    finally:
        await p.stop()


# ─── reconnects ──────────────────────────────────────────────────────────────


@pytest.mark.parametrize("wire", WIRES)
async def test_a_dropped_link_fails_calls_fast_and_a_redial_resumes_the_watch(
    pair_of, wire
):
    p = await pair_of(wire)
    target = f"{A}|{p.ai}"
    try:
        q = p.kb.ctx.ensure_inbox("watcher")
        assert p.kb.watch(target, "watcher") is None
        await p.until_watched(p.ai, True)

        # a call in flight when the link drops fails at once, `bridge_down`
        inflight = asyncio.create_task(p.call("probe", {"type": "hold"}))
        await asyncio.wait_for(_held[0].wait(), 3)
        await p.cut()
        failed = (await asyncio.wait_for(inflight, 3))["result"]
        assert failed["reason"] == "transport_error", failed
        assert failed["error"].startswith("bridge_down:"), failed
        # while the link is down, what A's AI streams is not kept for B
        assert (await _op(p.ka, p.ai, _turn("unseen")))["status"] == "completed"

        # A redials and says hello again: B's watch resumes without B asking
        await p.dial()
        await p.until_watched(p.ai, True)
        assert _drain(q) == [], "replayed"
        out = await p.call(p.ai, _turn("again"))
        assert out["status"] == "completed", out
        _assert_stream_before_done(_drain(q), wire)
    finally:
        await p.stop()


@pytest.mark.parametrize("wire", WIRES)
async def test_a_second_hello_for_the_guid_replaces_the_first_connection(pair_of, wire):
    p = await pair_of(wire)
    try:
        newer = _Raw()
        serving = asyncio.create_task(ws_run(newer, p.kb, p.kb.id, "b_ws"))
        newer.push({"type": "hello", "guid": A, "leg": "newer"})
        # the older connection is closed: A's leg sees its link go
        await _until(lambda: not p.reflect_connected())
        assert len(p.kb.ctx.peers[A]) == 1
        # and B's calls to A's guid ride the newer one
        call = asyncio.create_task(p.call("probe", {"type": "echo", "text": "n"}))
        frame = await newer.next("call")
        assert frame["target"] == "probe", frame
        newer.push({"type": "reply", "id": frame["id"], "data": {"from": "newer"}})
        assert (await asyncio.wait_for(call, 3))["result"] == {"from": "newer"}
        await newer.close()
        await asyncio.wait_for(serving, 3)
        assert A not in p.kb.ctx.peers
    finally:
        await p.stop()
