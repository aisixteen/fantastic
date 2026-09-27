"""The ONE watch path (connect-time auto-watch + explicit `watch` frame): the name
resolves the way `send` resolves a target (an id, a declared alias, `kernel`), the
watch runs under the leg's own identity, and a refusal is an `error` frame — never
silence, and never recorded as watched. Measured before: a socket opened by ALIAS
(`/exec/ws` for `exec_real`) mirrored nothing and said nothing, and the explicit
watch that followed was skipped as a duplicate."""

from __future__ import annotations

import json
import re
import sys
import time

from fastapi.testclient import TestClient
from web.app import make_app
from web_ws.tools import _make_endpoint

BASE = {"$self": "*", "$parent": "*", "$children": "*"}
PY = sys.executable


def _client(kernel, leg_id):
    app = make_app("test_web", kernel)
    app.add_api_websocket_route("/{host_id}/ws", _make_endpoint(leg_id, kernel))
    return TestClient(app)


def _until(ws, call_id, n=40):
    out = []
    for _ in range(n):
        m = json.loads(ws.receive_text())
        out.append(m)
        if m.get("type") == "reply" and m.get("id") == call_id:
            return out
    raise AssertionError(f"no reply {call_id}: {out}")


def _exec(k, *, grant: bool):
    k.create(
        "web_ws.tools",
        id="leg",
        ingress_rule="allow_all_ingress",
        can_call={**BASE, "exec_real": "*"} if grant else dict(BASE),
    )
    k.create(
        "exec_bridge.tools",
        id="exec_real",
        alias="exec",
        ingress_rule="allow_all_ingress",
        allowed=[re.escape(PY) + r" -c .*"],
        callable_by={**BASE, "leg": "*"} if grant else dict(BASE),
    )


def test_a_socket_opened_by_alias_receives_the_agents_events(seeded_kernel):
    _exec(seeded_kernel, grant=True)
    with _client(seeded_kernel, "leg") as c, c.websocket_connect("/exec/ws") as ws:
        ws.send_text(
            json.dumps(
                {
                    "type": "call",
                    "target": "exec",
                    "id": "1",
                    "payload": {
                        "type": "start",
                        "command": PY,
                        "args": ["-c", "print('v-42')"],
                    },
                }
            )
        )
        frames = _until(ws, "1")
        time.sleep(1.0)
        ws.send_text(
            json.dumps(
                {
                    "type": "call",
                    "target": "exec",
                    "id": "2",
                    "payload": {"type": "status"},
                }
            )
        )
        frames += _until(ws, "2")
    kinds = [f["payload"].get("type") for f in frames if f.get("type") == "event"]
    assert "job_started" in kinds and "job_done" in kinds, frames
    assert not [f for f in frames if f.get("type") == "error"], frames


def test_a_refused_auto_watch_is_an_error_frame_not_silence(seeded_kernel):
    _exec(seeded_kernel, grant=False)
    with _client(seeded_kernel, "leg") as c, c.websocket_connect("/exec/ws") as ws:
        first = json.loads(ws.receive_text())
        assert first["type"] == "error" and first["reason"] == "unauthorized", first
        assert not seeded_kernel.ctx.agents["exec_real"]._watcher_ids, "not recorded"
        # …and an explicit watch is re-tried, not skipped as a duplicate
        ws.send_text(json.dumps({"type": "watch", "src": "exec", "id": "w"}))
        again = json.loads(ws.receive_text())
        assert again["type"] == "error" and again["id"] == "w", again


def test_an_unknown_name_is_an_error_frame(seeded_kernel):
    seeded_kernel.create("web_ws.tools", id="leg", ingress_rule="allow_all_ingress")
    with _client(seeded_kernel, "leg") as c, c.websocket_connect("/nobody/ws") as ws:
        first = json.loads(ws.receive_text())
    assert first["type"] == "error" and first["reason"] == "unknown_agent", first
