"""C24 for the TOKEN stream — an AI generation watched through the WS surface.

The closest shape to what a session hub actually consumes: a browser holds a WS
to the kernel, watches the AI agent, sends it a message, and receives the
generation as `event` frames — `token` chunks then the terminal
`status(phase='done')`. Asserted the C24 way: chunk order preserved exactly,
terminal last, nothing model-facing reordered by the wire.

Zero kernel changes; composes the existing pieces — ai_core's token routing
(non-cli client_id ⇒ the AI's own inbox), web_ws's watch + event forwarding,
and a scripted provider so the chunk sequence is known."""

from __future__ import annotations

from _testkit import memory_meta
import json

import pytest
from fastapi.testclient import TestClient

from ai_core import core
from web.app import make_app
from web_ws.tools import _make_endpoint

CHUNKS = [f"tok{i} " for i in range(12)]


class _ChunkProvider:
    async def chat(self, messages, max_tokens=None):
        for c in CHUNKS:
            yield c


@pytest.fixture
def wired(seeded_kernel, store_agent):
    app = make_app("test_web", seeded_kernel)
    seeded_kernel.create(
        "web_ws.tools",
        id="c24_ws_leg",
        ingress_rule="allow_all_ingress",
        can_call={"c24_ai": "*"},
    )
    app.add_api_websocket_route(
        "/{host_id}/ws", _make_endpoint("c24_ws_leg", seeded_kernel)
    )
    seeded_kernel.create(
        "ollama_backend.tools",
        id="c24_ai",
        context_window=32768,
        **memory_meta(seeded_kernel, "c24_ai", {"cli": "*"}),
        callable_by={"c24_ws_leg": "*"},
    )
    core._providers["c24_ai"] = _ChunkProvider()
    try:
        with TestClient(app) as c:
            yield c
    finally:
        core._providers.pop("c24_ai", None)


def test_token_stream_order_and_terminal_across_ws(wired):
    ws_ctx = wired.websocket_connect("/kernel_state/ws")
    with ws_ctx as ws:
        # subscribe to the AI's inbox — where non-cli token events land
        ws.send_text(json.dumps({"type": "watch", "src": "c24_ai"}))
        # drive one message AS the browser (client_id != cli ⇒ tokens to inbox)
        ws.send_text(
            json.dumps(
                {
                    "type": "call",
                    "target": "c24_ai",
                    "payload": {"type": "send", "text": "go", "client_id": "tab1"},
                    "id": "t1",
                }
            )
        )
        tokens: list[str] = []
        done = False
        reply = None
        for _ in range(200):
            msg = json.loads(ws.receive_text())
            if msg.get("type") == "event":
                ev = msg.get("payload") or {}
                if ev.get("type") == "token":
                    assert not done, "a token arrived AFTER the terminal"
                    tokens.append(ev.get("text"))
                elif ev.get("type") == "status" and ev.get("phase") == "done":
                    done = True
            elif msg.get("type") == "reply" and msg.get("id") == "t1":
                reply = msg
            if done and reply is not None:
                break
        assert tokens == CHUNKS, "the wire reordered or dropped token chunks"
        assert done, "no terminal status(done) arrived"
        assert reply["data"]["final"] == "".join(CHUNKS).strip() or reply["data"].get(
            "response"
        )
