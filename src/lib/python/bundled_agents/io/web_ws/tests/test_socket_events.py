"""The leg tells its watchers when a socket opens and when it closes, so the side
that was dialed (a box a master reaches over a tunnel) knows its link without a
call: `socket_open {client_id}`, then `socket_closed {client_id, guid}`."""

from __future__ import annotations

import json
import time

from fastapi.testclient import TestClient
from web.app import make_app
from web_ws.tools import _make_endpoint


def _events(q, ty, timeout=5.0):
    end = time.time() + timeout
    while time.time() < end:
        while not q.empty():
            p = q.get_nowait()
            if p.get("type") == ty:
                return p
        time.sleep(0.05)
    raise AssertionError(f"no {ty} in time")


def test_the_leg_emits_socket_open_and_socket_closed(seeded_kernel):
    k = seeded_kernel
    k.create("web_ws.tools", id="leg", ingress_rule="allow_all_ingress")
    k.get("test_web") or k.create(None, id="test_web", port=8088)
    q = k.ctx.ensure_inbox("leg_watcher")
    k.watch("leg", "leg_watcher")
    app = make_app("test_web", k)
    app.add_api_websocket_route("/{host_id}/ws", _make_endpoint("leg", k))
    with TestClient(app, headers={"host": "127.0.0.1:8088"}) as c:
        with c.websocket_connect("/test_web/ws") as ws:
            opened = _events(q, "socket_open")
            assert opened["client_id"], opened
            ws.send_text(json.dumps({"type": "hello", "guid": "g-peer"}))
            time.sleep(0.2)
        closed = _events(q, "socket_closed")
    assert closed["client_id"] == opened["client_id"], closed
    assert closed["guid"] == "g-peer", closed
