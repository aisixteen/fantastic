"""The ONE watch path (connect-time auto-watch + explicit `watch` frame): the name
resolves the way `send` resolves a target (an id or `kernel`), the watch runs under
the leg's own identity, and a refusal is an `error` frame — never silence, and never
recorded as watched (a connect-time watch that did not resolve, recorded anyway, would
mirror nothing, say nothing, and make the explicit watch that follows skip as a
duplicate)."""

from __future__ import annotations

import json
import sys
import time

from _testkit import command_rx
from fastapi.testclient import TestClient
from web.app import make_app
from web_ws.tools import _make_endpoint

BASE = {"$self": "*", "$parent": "*", "$subtree": "*"}
PY = sys.executable


def _client(kernel, leg_id):
    # The web host's record: its port is the authority the Host guard admits
    # — the client names it on every request, the upgrade included.
    kernel.get("test_web") or kernel.create(None, id="test_web", port=8088)
    app = make_app("test_web", kernel)
    app.add_api_websocket_route("/{host_id}/ws", _make_endpoint(leg_id, kernel))
    return TestClient(app, headers={"host": "127.0.0.1:8088"})


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
        can_call={**BASE, "exec": "*"} if grant else dict(BASE),
    )
    k.create(
        "exec_bridge.tools",
        id="exec",
        ingress_rule="allow_all_ingress",
        allowed=[command_rx(PY) + r" -c .*"],
        cwd=".",
        callable_by={**BASE, "leg": "*"} if grant else dict(BASE),
    )


def test_a_socket_opened_on_an_agent_receives_its_events(seeded_kernel):
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
    # every mirrored event names the agent it came from — the name the socket
    # watched — so a bridge on the other end re-emits only what it subscribed to
    events = [f for f in frames if f.get("type") == "event"]
    assert events and all(f.get("src") == "exec" for f in events), events
    assert list(events[0]) == ["type", "src", "payload"], events[0]


def test_a_refused_auto_watch_is_an_error_frame_not_silence(seeded_kernel):
    _exec(seeded_kernel, grant=False)
    with _client(seeded_kernel, "leg") as c, c.websocket_connect("/exec/ws") as ws:
        first = json.loads(ws.receive_text())
        assert first["type"] == "error" and first["reason"] == "unauthorized", first
        assert not seeded_kernel.ctx.agents["exec"]._watcher_ids, "not recorded"
        # …and an explicit watch is re-tried, not skipped as a duplicate
        ws.send_text(json.dumps({"type": "watch", "src": "exec", "id": "w"}))
        again = json.loads(ws.receive_text())
        assert again["type"] == "error" and again["id"] == "w", again


def test_an_unknown_name_is_an_error_frame(seeded_kernel):
    seeded_kernel.create("web_ws.tools", id="leg", ingress_rule="allow_all_ingress")
    with _client(seeded_kernel, "leg") as c, c.websocket_connect("/nobody/ws") as ws:
        first = json.loads(ws.receive_text())
    assert first["type"] == "error" and first["reason"] == "unknown_agent", first


# ─── the ws watch proxy, pinned (reduction candidate H: one mirror) ──────────


def _call_and_wait(ws, cid, target="exec"):
    ws.send_text(
        json.dumps(
            {
                "type": "call",
                "target": target,
                "id": cid,
                "payload": {"type": "reflect"},
            }
        )
    )
    return _until(ws, cid)


def test_the_unknown_name_error_frame_is_exact(seeded_kernel):
    seeded_kernel.create("web_ws.tools", id="leg", ingress_rule="allow_all_ingress")
    with _client(seeded_kernel, "leg") as c, c.websocket_connect("/nobody/ws") as ws:
        first = json.loads(ws.receive_text())
    assert first == {
        "type": "error",
        "id": "",
        "error": "watch: no agent 'nobody' (an id, or `kernel`)",
        "reason": "unknown_agent",
        "src": "nobody",
    }


def test_an_unwatch_frame_releases_the_reader_and_the_drain(seeded_kernel):
    k = seeded_kernel
    _exec(k, grant=True)
    before = set(k.ctx.inboxes)
    with _client(k, "leg") as c, c.websocket_connect("/exec/ws") as ws:
        _call_and_wait(ws, "1")
        readers = [i for i in k.ctx.inboxes if i.endswith(">exec")]
        assert len(readers) == 1, readers
        assert readers[0] in k.ctx.agents["exec"]._watcher_ids
        ws.send_text(json.dumps({"type": "unwatch", "src": "exec"}))
        frames = _call_and_wait(ws, "2")
        assert not [i for i in k.ctx.inboxes if i.endswith(">exec")]
        assert not (k.ctx.agents["exec"]._watcher_ids & set(readers))
        # an explicit watch after the unwatch registers afresh
        ws.send_text(json.dumps({"type": "watch", "src": "exec", "id": "w"}))
        frames += _call_and_wait(ws, "3")
        assert len([i for i in k.ctx.inboxes if i.endswith(">exec")]) == 1
    assert not [f for f in frames if f.get("type") == "error"], frames
    # the socket closed: every reader it held, and its own client inbox, is gone
    for _ in range(100):
        if set(k.ctx.inboxes) == before:
            break
        time.sleep(0.01)
    assert set(k.ctx.inboxes) == before
    assert not k.ctx.agents["exec"]._watcher_ids - set(before)


def test_a_watch_of_kernel_resolves_to_the_root_and_frames_its_name(seeded_kernel):
    k = seeded_kernel
    k.create(
        "web_ws.tools",
        id="leg",
        ingress_rule="allow_all_ingress",
        can_call={**BASE, k.id: ["watch", "reflect"]},
    )
    k.ctx.update(k.id, callable_by={**BASE, "leg": ["watch", "reflect"]})
    with _client(k, "leg") as c, c.websocket_connect("/kernel/ws") as ws:
        frames = _call_and_wait(ws, "1", target="kernel")
        readers = [i for i in k.ctx.inboxes if i.endswith(f">{k.id}")]
        assert len(readers) == 1, readers
        assert readers[0] in k.ctx.agents[k.id]._watcher_ids
        # the mirrored reflect is produced during the dispatch; its frame may
        # trail the reply, so read on until it is in hand
        for _ in range(10):
            if any(f.get("type") == "event" for f in frames):
                break
            frames.append(json.loads(ws.receive_text()))
    events = [f for f in frames if f.get("type") == "event"]
    # the event is stamped with the NAME the socket watched, not the resolved id
    assert events and all(f["src"] == "kernel" for f in events), frames
    assert events[0]["payload"] == {"type": "reflect"}
