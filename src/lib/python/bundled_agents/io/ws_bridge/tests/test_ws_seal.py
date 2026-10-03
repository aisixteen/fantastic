"""A ws leg to a NON-LOOPBACK host is SEALED for secrets (owner, 2026-10-03,
audit S8).

`ws://` carries every frame in clear, and a forward's secret values ride its frame
verbatim (the far kernel needs them). So a leg dialed to a host other than this
machine (`localhost` / `127.0.0.1` / `[::1]` — an ssh tunnel's local end) refuses
any call carrying secret marks, in both directions, with the relay seal's shape:
outbound, a marked `forward` is refused before a frame leaves; inbound, a `call`
frame naming `secret` is answered with the same refusal and never dispatched.
"""

from __future__ import annotations

import asyncio
import json

import pytest
from websockets.protocol import State

from io_bridge import _engine
from ws_bridge import _ws
from ws_bridge._ws import SECRET_SEALED, SECRET_SEALED_HINT, WSTransport

KEY = "sk-ws-seal-0123456789abcdef"
_SEALED = {"error": SECRET_SEALED, "reason": "unauthorized", "hint": SECRET_SEALED_HINT}


class _FakeWS:
    """A `websockets` client: records what is sent (decoded), answers every `call`
    frame with `{ok: true}`, and hands back what a test puts in `inbox`."""

    def __init__(self) -> None:
        self.sent: list[dict] = []
        self.inbox: asyncio.Queue = asyncio.Queue()
        self.state = State.OPEN

    async def send(self, wire) -> None:
        frame = json.loads(wire)
        self.sent.append(frame)
        if frame.get("type") == "call":
            self.inbox.put_nowait(
                json.dumps({"type": "reply", "id": frame["id"], "data": {"ok": True}})
            )

    async def recv(self):
        return await self.inbox.get()

    async def close(self) -> None:
        self.state = State.CLOSED


@pytest.fixture
def dials(monkeypatch):
    """Every dial `build_transport` makes: `(url, sealed, fake socket)`."""
    out: list[tuple[str, bool, _FakeWS]] = []

    async def _connect(url, *, sealed):
        ws = _FakeWS()
        out.append((url, sealed, ws))
        return WSTransport(ws, sealed=sealed)

    monkeypatch.setattr(_ws.WSTransport, "connect", staticmethod(_connect))
    yield out
    _engine._bridges.clear()


async def _leg(k, host: str) -> str:
    out = (
        await k.send(
            [
                {
                    "target": k.id,
                    "payload": {
                        "type": "create_agent",
                        "parent_id": k.id,
                        "handler_module": "ws_bridge.tools",
                        "host": host,
                        "port": 8088,
                        "peer_id": "web_ws",
                    },
                }
            ]
        )
    )[0]["result"]
    leg = out["id"]
    # create_agent boots the leg; a second boot answers `already`
    booted = (await k.send([{"target": leg, "payload": {"type": "boot"}}]))[0]
    assert booted["result"] == {"already": True, "transport": "ws"}, booted
    return leg


async def _forward(k, leg: str, **item) -> dict:
    return (
        await k.send(
            [
                {
                    "target": leg,
                    "payload": {
                        "type": "forward",
                        "target": "vault",
                        "payload": {"type": "set_key", "key": KEY},
                        "timeout": 2,
                    },
                    **item,
                }
            ]
        )
    )[0]


@pytest.mark.parametrize(
    ("host", "sealed"),
    [
        ("10.0.0.5", True),
        ("peer.example.test", True),
        ("0.0.0.0", True),
        ("localhost", False),
        ("127.0.0.1", False),
        ("[::1]", False),
    ],
)
async def test_the_dialed_host_decides_the_seal(seeded_kernel, dials, host, sealed):
    await _leg(seeded_kernel, host)
    assert [(u, s) for u, s, _ in dials] == [(f"ws://{host}:8088/web_ws/ws", sealed)]


@pytest.mark.parametrize(
    "secret",
    [
        ["payload.payload.key"],
        ["result"],
        ["payload.timeout"],
        [["payload", "payload", "key"]],
    ],
)
async def test_a_marked_forward_to_a_non_loopback_host_is_refused_before_a_frame_leaves(
    seeded_kernel, dials, secret
):
    k = seeded_kernel
    leg = await _leg(k, "10.0.0.5")
    out = await _forward(k, leg, secret=secret)
    assert out["status"] == "errored" and out["result"] == _SEALED, out
    ws = dials[0][2]
    assert ws.sent == [], "nothing crossed the network"
    # the same leg still carries an UNMARKED forward — the seal is about marks only
    plain = await _forward(k, leg)
    assert plain["result"] == {"ok": True}, plain
    assert [f["type"] for f in ws.sent] == ["call"] and "secret" not in ws.sent[0]


async def test_a_marked_forward_over_a_loopback_leg_carries_its_marks(
    seeded_kernel, dials
):
    k = seeded_kernel
    leg = await _leg(k, "127.0.0.1")
    out = await _forward(k, leg, secret=["payload.payload.key"])
    assert out["result"] == {"ok": True}, out
    (frame,) = dials[0][2].sent
    assert frame["secret"] == ["payload.key"] and frame["payload"]["key"] == KEY


@pytest.mark.parametrize(("sealed", "dispatched"), [(True, False), (False, True)])
async def test_an_inbound_marked_call_on_a_non_loopback_leg_is_answered_sealed(
    sealed, dispatched
):
    """Inbound: on a sealed leg a `call` frame naming `secret` is answered with
    the seal's refusal and never reaches the engine; the next frame is read on. A
    loopback leg hands the call to the engine."""
    ws = _FakeWS()
    t = WSTransport(ws, sealed=sealed)
    call = {
        "type": "call",
        "id": "sealed-1",
        "target": "vault",
        "payload": {"type": "set_key", "key": KEY},
        "caller": "peer-g|kernel",
        "secret": ["payload.key"],
    }
    after = {"type": "reply", "id": "x-1", "data": {"ok": True}}
    ws.inbox.put_nowait(json.dumps(call))
    ws.inbox.put_nowait(json.dumps(after))
    got = await asyncio.wait_for(t.recv(), 2)
    if dispatched:
        assert got == call and ws.sent == []
    else:
        assert got == after, got
        assert ws.sent == [{"type": "reply", "id": "sealed-1", "data": _SEALED}]


async def test_the_forward_doc_names_the_seal(seeded_kernel, dials):
    k = seeded_kernel
    leg = await _leg(k, "10.0.0.5")
    shown = (await k.send([{"target": leg, "payload": {"type": "reflect"}}]))[0]
    assert shown["result"]["verbs"]["forward"].endswith(
        " SEALED for secrets to a non-loopback host: a call carrying any secret mark "
        "is refused — ws:// carries every frame in clear."
    ), shown["result"]["verbs"]["forward"]
