"""The web host's closed defaults: bind LOOPBACK unless the record
names a `host`; refuse a FOREIGN browser origin on websocket upgrades and on
non-GET/HEAD/OPTIONS HTTP requests, unless the record's `allowed_origins` lists it;
and refuse EVERY request whose Host is not the host's own authority."""

import pytest
from fastapi import FastAPI, WebSocket
from fastapi.testclient import TestClient
from starlette.websockets import WebSocketDisconnect

from web.app import (
    DEFAULT_HOST,
    _OriginGuard,
    origin_allowed,
    own_authorities,
    own_origins,
)

#: The loopback default's own authority; TestClient's `testserver` is not one.
OWN = "http://127.0.0.1:8088"


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
    c = TestClient(_app({"port": 8088}), base_url=OWN)
    r = c.post("/x", headers={"Origin": "http://evil.example"})
    assert r.status_code == 403 and r.json()["reason"] == "unauthorized"
    assert c.post("/x", headers={"Origin": "http://localhost:8088"}).status_code == 200
    assert c.post("/x").status_code == 200
    assert c.get("/x", headers={"Origin": "http://evil.example"}).status_code == 200


def test_foreign_websocket_upgrade_is_refused_listed_origin_passes():
    c = TestClient(
        _app({"port": 8088, "allowed_origins": ["https://ui.example"]}),
        headers={"host": "127.0.0.1:8088"},
    )
    with pytest.raises(WebSocketDisconnect):
        with c.websocket_connect(
            "/ws", headers={"Origin": "http://evil.example"}
        ) as ws:
            ws.receive_text()
    with c.websocket_connect("/ws", headers={"Origin": "https://ui.example"}) as ws:
        assert ws.receive_text() == "hi"


# ─── the Host guard ────────────────────────────────────────────────────────────


def test_own_authorities_are_the_bound_host_port_plus_loopback_spellings():
    assert own_authorities({"port": 8088}) == {
        "127.0.0.1:8088",
        "localhost:8088",
        "[::1]:8088",
    }
    assert own_authorities({"host": "localhost", "port": 8088}) == {
        "127.0.0.1:8088",
        "localhost:8088",
        "[::1]:8088",
    }
    # a non-loopback bind answers its own host:port and nothing else
    assert own_authorities({"host": "0.0.0.0", "port": 8088}) == {"0.0.0.0:8088"}
    assert own_authorities({"host": "192.168.1.5", "port": 8088}) == {
        "192.168.1.5:8088"
    }
    # an IPv6 literal is bracketed, as a Host header spells it
    assert own_authorities({"host": "::", "port": 8088}) == {"[::]:8088"}
    # no Host header at all is not an authority
    assert "" not in own_authorities({"port": 8088})
# twin: a_rebound_host_cannot_read_the_file_proxy
# twin: a_foreign_host_is_refused_on_every_method_with_no_origin


def test_rebound_host_is_refused_on_every_method_with_no_origin(capsys):
    """THE REBINDING READ: a page whose domain rebinds to 127.0.0.1 sends a
    same-origin GET — no Origin header — naming its own domain in Host."""
    c = TestClient(_app({"port": 8088}), base_url=OWN)
    for method in ("GET", "HEAD", "OPTIONS", "POST"):
        r = c.request(method, "/x", headers={"Host": "rebind.invalid:8088"})
        assert r.status_code == 403, method
        if method != "HEAD":
            assert r.json() == {
                "error": "host 'rebind.invalid:8088' is not this server's own authority",
                "reason": "unauthorized",
                "hint": "address the host by its bound host:port (a loopback bind "
                "also answers localhost, 127.0.0.1 and [::1] on that port)",
            }
    assert (
        "  [web] web: refused http /x for foreign host 'rebind.invalid:8088'"
        in capsys.readouterr().out
    )


def test_loopback_spellings_pass_another_port_does_not():
    c = TestClient(_app({"port": 8088}), base_url=OWN)
    for host in ("127.0.0.1:8088", "localhost:8088", "[::1]:8088"):
        assert c.get("/x", headers={"Host": host}).status_code == 200, host
    for host in ("127.0.0.1:9999", "localhost", "127.0.0.1"):
        assert c.get("/x", headers={"Host": host}).status_code == 403, host


def test_a_published_bind_admits_only_its_own_host_port():
    """`0.0.0.0` publishes the port, but the only Host it answers is `0.0.0.0:<port>`
    — a port map, a LAN name, a tunnel name, even the loopback spellings are other
    authorities."""
    c = TestClient(_app({"host": "0.0.0.0", "port": 8088}), base_url=OWN)
    assert c.get("/x", headers={"Host": "0.0.0.0:8088"}).status_code == 200
    for host in (
        "127.0.0.1:8088",
        "localhost:8088",
        "192.168.1.5:8088",
        "box.lan:8088",
    ):
        assert c.get("/x", headers={"Host": host}).status_code == 403, host


def test_a_rebound_websocket_upgrade_is_refused():
    """TestClient's websocket_connect always dials `ws://testserver`; the Host
    header is what the upgrade names."""
    c = TestClient(_app({"port": 8088}))
    with pytest.raises(WebSocketDisconnect):
        with c.websocket_connect("/ws", headers={"host": "rebind.invalid:8088"}) as ws:
            ws.receive_text()
    with c.websocket_connect("/ws", headers={"host": "127.0.0.1:8088"}) as ws:
        assert ws.receive_text() == "hi"


def test_host_edit_applies_without_a_restart():
    rec = {"port": 8088}
    c = TestClient(_app(rec), base_url=OWN)
    assert c.get("/x").status_code == 200
    rec["port"] = 9090
    assert c.get("/x").status_code == 403
    assert c.get("/x", headers={"Host": "localhost:9090"}).status_code == 200
