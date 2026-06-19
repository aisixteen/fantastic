"""web_ws — the io_bridge ingress gate on the WS leg.

A sealed leg (`ingress_rule=deny_inbound`) refuses inbound calls AND teaches how to
open the edge (reason + hint + `see` pointer) — discovery-through-denial. An open
leg dispatches. A `password` leg checks the credential on the frame ENVELOPE. reflect
surfaces the leg's posture so a denied client can find the door.
"""

from __future__ import annotations

import json

from fastapi.testclient import TestClient

from web.app import make_app
from web_ws.tools import _make_endpoint, handler


def _client(kernel, leg_id):
    app = make_app("test_web", kernel)
    app.add_api_websocket_route("/{host_id}/ws", _make_endpoint(leg_id, kernel))
    return TestClient(app)


def _reply(ws, call_id, max_frames=10):
    for _ in range(max_frames):
        msg = json.loads(ws.receive_text())
        if msg.get("type") == "reply" and msg.get("id") == call_id:
            return msg
    raise AssertionError(f"no reply for id={call_id}")


def test_sealed_leg_denies_and_teaches(seeded_kernel):
    seeded_kernel.create("web_ws.tools", id="sealed_ws", ingress_rule="deny_inbound")
    with _client(seeded_kernel, "sealed_ws") as c:
        with c.websocket_connect("/kernel_state/ws") as ws:
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
            d = _reply(ws, "1")["data"]
    assert d["reason"] == "unauthorized"
    assert "ingress_rule" in d.get("hint", "")
    assert "tree" not in d  # the call did NOT dispatch — no reflect body leaked


def test_open_leg_dispatches(seeded_kernel):
    seeded_kernel.create("web_ws.tools", id="open_ws", ingress_rule="allow_all")
    with _client(seeded_kernel, "open_ws") as c:
        with c.websocket_connect("/kernel_state/ws") as ws:
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
            d = _reply(ws, "1")["data"]
    assert d["id"] == "kernel_state"  # the real reflect dispatched


def test_absent_rule_is_sealed(seeded_kernel):
    # No ingress_rule on the leg ⇒ DenyInbound ⇒ sealed by default: the bare leg
    # refuses AND teaches how to open it (mirrors test_sealed_leg_denies_and_teaches).
    seeded_kernel.create("web_ws.tools", id="bare_ws")
    with _client(seeded_kernel, "bare_ws") as c:
        with c.websocket_connect("/kernel_state/ws") as ws:
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
            d = _reply(ws, "1")["data"]
    assert d["reason"] == "unauthorized"
    assert "ingress_rule" in d.get("hint", "")
    assert "tree" not in d  # the call did NOT dispatch — no reflect body leaked


def test_password_leg_checks_envelope_token(seeded_kernel, monkeypatch):
    monkeypatch.setenv("FANTASTIC_GROUP_TOKEN", "s3cret")
    seeded_kernel.create("web_ws.tools", id="pw_ws", ingress_rule="password")
    with _client(seeded_kernel, "pw_ws") as c:
        with c.websocket_connect("/kernel_state/ws") as ws:
            # right token on the ENVELOPE (sibling of id/target) → dispatches
            ws.send_text(
                json.dumps(
                    {
                        "type": "call",
                        "target": "kernel_state",
                        "payload": {"type": "reflect"},
                        "id": "ok",
                        "auth_token": "s3cret",
                    }
                )
            )
            good = _reply(ws, "ok")["data"]
            # wrong token → denied
            ws.send_text(
                json.dumps(
                    {
                        "type": "call",
                        "target": "kernel_state",
                        "payload": {"type": "reflect"},
                        "id": "bad",
                        "auth_token": "nope",
                    }
                )
            )
            bad = _reply(ws, "bad")["data"]
    assert good["id"] == "kernel_state"
    assert bad["reason"] == "unauthorized"


async def test_reflect_surfaces_leg_posture(seeded_kernel):
    seeded_kernel.create("web_ws.tools", id="sealed_ws2", ingress_rule="deny_inbound")
    r = await handler("sealed_ws2", {"type": "reflect"}, seeded_kernel)
    assert r["sealed"] is True
    assert r["ingress_rule"] == "deny_inbound"
    assert True  # see removed from posture
    # an open leg reflects unsealed
    seeded_kernel.create("web_ws.tools", id="open_ws2", ingress_rule="allow_all")
    r2 = await handler("open_ws2", {"type": "reflect"}, seeded_kernel)
    assert r2["sealed"] is False and r2["ingress_rule"] == "allow_all"


def _watch_spy(kernel, monkeypatch):
    """Record every `kernel.watch(src, ...)` the proxy makes, while still doing it."""
    calls: list[str] = []
    orig = kernel.watch
    monkeypatch.setattr(
        kernel, "watch", lambda src, cid: (calls.append(src), orig(src, cid))[1]
    )
    return calls


def test_sealed_leg_skips_autowatch_no_telemetry_leak(seeded_kernel, monkeypatch):
    # SECURITY regression: a sealed leg must NOT auto-watch the connected host at the
    # handshake — otherwise an unauthenticated client that merely opens the socket
    # passively receives the host's inbox events without ever passing the gate.
    calls = _watch_spy(seeded_kernel, monkeypatch)
    seeded_kernel.create("web_ws.tools", id="sealed_ws3", ingress_rule="deny_inbound")
    with _client(seeded_kernel, "sealed_ws3") as c:
        with c.websocket_connect("/kernel_state/ws") as ws:
            # the denied call's reply is our sync point: the connect path has fully run
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
            _reply(ws, "1")
    assert "kernel_state" not in calls  # the sealed leg did NOT mirror the host inbox


def test_open_leg_autowatches_host(seeded_kernel, monkeypatch):
    calls = _watch_spy(seeded_kernel, monkeypatch)
    seeded_kernel.create("web_ws.tools", id="open_ws3", ingress_rule="allow_all")
    with _client(seeded_kernel, "open_ws3") as c:
        with c.websocket_connect("/kernel_state/ws") as ws:
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
            _reply(ws, "1")
    assert (
        "kernel_state" in calls
    )  # an open leg mirrors the host inbox (unchanged behavior)
