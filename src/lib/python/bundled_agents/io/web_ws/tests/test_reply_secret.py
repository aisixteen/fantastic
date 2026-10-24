"""A call's reply frame carries the outcome's secret marks: a credential store's
`get` reply names its field values as `secret`, so the far end of the socket (a
bridge that forwards it) can record them as `[secret]`. The reply data is whole."""

from __future__ import annotations

import json

from _testkit import wire
from web.app import make_app
from web_ws.tools import _make_endpoint

SECRET = "sk-REPLY-9c1d"


def _client(kernel, leg_id):
    kernel.get("test_web") or kernel.create(None, id="test_web", port=8088)
    app = make_app("test_web", kernel)
    app.add_api_websocket_route("/{host_id}/ws", _make_endpoint(leg_id, kernel))
    from starlette.testclient import TestClient

    return TestClient(app, headers={"host": "127.0.0.1:8088"})


def _reply(ws, call_id, max_frames=10) -> dict:
    for _ in range(max_frames):
        msg = json.loads(ws.receive_text())
        if msg.get("type") == "reply" and msg.get("id") == call_id:
            return msg
    raise AssertionError(f"no reply for id={call_id}")


async def test_a_call_reply_frame_carries_the_outcomes_secret_marks(
    seeded_kernel, tmp_path
):
    k = seeded_kernel
    k.ctx.update(k.id, guid="g1")
    k.create("web_ws.tools", id="leg", ingress_rule="allow_all_ingress")
    k.create(
        "credential_store.tools",
        id="creds",
        file=str(tmp_path / "creds.yaml"),
        callable_by={"leg": "*"},
    )
    wire(k, "leg", "creds")
    await k.send(
        [
            {
                "target": "kernel",
                "payload": {
                    "type": "update_agent",
                    "id": "creds",
                    "refs": {
                        "svc/main": {"host": "api.example", "fields": ["api_key"]}
                    },
                },
            }
        ]
    )
    stored = (
        await k.send(
            [
                {
                    "target": "creds",
                    "payload": {
                        "type": "set",
                        "ref": "svc/main",
                        "fields": {"api_key": SECRET},
                    },
                }
            ]
        )
    )[0]
    assert stored["status"] == "completed", stored

    with _client(k, "leg") as c:
        with c.websocket_connect("/kernel_state/ws") as ws:
            ws.send_text(
                json.dumps(
                    {
                        "type": "call",
                        "target": "creds",
                        "payload": {"type": "get", "ref": "svc/main"},
                        "id": "get-1",
                    }
                )
            )
            got = _reply(ws, "get-1")
            assert got["data"]["fields"]["api_key"] == SECRET, got
            assert got["secret"] == ["fields.api_key"], got

            ws.send_text(
                json.dumps(
                    {
                        "type": "call",
                        "target": "creds",
                        "payload": {"type": "list"},
                        "id": "list-1",
                    }
                )
            )
            plain = _reply(ws, "list-1")
            assert "secret" not in plain, plain
