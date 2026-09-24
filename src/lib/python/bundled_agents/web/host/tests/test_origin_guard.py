"""The web host's two closed defaults (2026-09-24): bind LOOPBACK unless the record
names a `host`; refuse a FOREIGN browser origin on websocket upgrades and on
non-GET/HEAD/OPTIONS HTTP requests, unless the record's `allowed_origins` lists it."""

import pytest
from fastapi import FastAPI, WebSocket
from fastapi.testclient import TestClient
from starlette.websockets import WebSocketDisconnect

from web.app import (
    DEFAULT_HOST,
    _OriginGuard,
    origin_allowed,
    own_origins,
)


class _K:
    def __init__(self, rec):
        self.rec = rec

    def get(self, _id):
        return self.rec


def _app(rec):
    app = FastAPI()

    @app.post("/x")
    async def post():
        return {"ok": True}

    @app.get("/x")
    async def get():
        return {"ok": True}

    @app.websocket("/ws")
    async def ws(websocket: WebSocket):
        await websocket.accept()
        await websocket.send_text("hi")
        await websocket.close()

    app.add_middleware(_OriginGuard, web_agent_id="web", kernel=_K(rec))
    return app


def test_loopback_is_the_default_and_its_spellings_are_own():
    assert DEFAULT_HOST == "127.0.0.1"
    own = own_origins({"port": 8088})
    assert {"http://127.0.0.1:8088", "http://localhost:8088"} <= own
    assert own_origins({"host": "0.0.0.0", "port": 8088}) == {"http://0.0.0.0:8088"}


def test_origin_rule():
    rec = {"port": 8088}
    assert origin_allowed(rec, None)  # a non-browser client
    assert origin_allowed(rec, "http://localhost:8088")
    assert not origin_allowed(rec, "http://evil.example:8088")
    assert not origin_allowed(rec, "http://localhost:9999")
    assert origin_allowed(
        {**rec, "allowed_origins": ["https://ui.example"]}, "https://ui.example"
    )


def test_foreign_post_is_403_own_and_originless_pass_get_is_not_gated():
    c = TestClient(_app({"port": 8088}))
    r = c.post("/x", headers={"Origin": "http://evil.example"})
    assert r.status_code == 403 and r.json()["reason"] == "unauthorized"
    assert c.post("/x", headers={"Origin": "http://localhost:8088"}).status_code == 200
    assert c.post("/x").status_code == 200
    assert c.get("/x", headers={"Origin": "http://evil.example"}).status_code == 200


def test_foreign_websocket_upgrade_is_refused_listed_origin_passes():
    c = TestClient(_app({"port": 8088, "allowed_origins": ["https://ui.example"]}))
    with pytest.raises(WebSocketDisconnect):
        with c.websocket_connect(
            "/ws", headers={"Origin": "http://evil.example"}
        ) as ws:
            ws.receive_text()
    with c.websocket_connect("/ws", headers={"Origin": "https://ui.example"}) as ws:
        assert ws.receive_text() == "hi"
