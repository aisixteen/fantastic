"""cloud_bridge — token, TLS 1.3 mutual-auth E2E, relay transport framing, and the
engine-integration forward (proves the bundle reuses io_bridge).

No live relay / no network: the TLS handshake runs over a `FakeWS` pair that mimics
the relay forwarding binary frames verbatim, and the forward round-trip uses a
MemoryTransport injection (same seam as ws_bridge).
"""

from __future__ import annotations

import asyncio

import pytest
from cryptography.hazmat.primitives import serialization
from cryptography.hazmat.primitives.asymmetric.ed25519 import Ed25519PrivateKey

from _testkit import boot_root
from io_bridge import _engine as core
from io_bridge import MemoryTransport
from io_bridge.ingress_rules import DenyInbound, Password
from cloud_bridge import tools as cb
from cloud_bridge._tls import peer_pubkey_from_der, self_signed_cert
from cloud_bridge._token import (
    MAX_TTL,
    b64url_decode,
    build_claims,
    decode_token_claims,
    encode_dev_token,
    encode_signed_token,
)
from cloud_bridge._transport import KEEPALIVE_TYPE, CloudBridgeTransport


def _ed() -> bytes:
    return Ed25519PrivateKey.generate().private_bytes(
        serialization.Encoding.Raw,
        serialization.PrivateFormat.Raw,
        serialization.NoEncryption(),
    )


def _pub(priv: bytes) -> bytes:
    return Ed25519PrivateKey.from_private_bytes(priv).public_key().public_bytes_raw()


# ─── _token ──────────────────────────────────────────────────────


def test_dev_token_round_trips_claims():
    claims = build_claims(
        tenant_id="t1", peer_id="A", rendezvous="rv", partner_peer_id="B", now=1000
    )
    tok = encode_dev_token(claims)
    assert "." not in tok  # dev token is the claims segment only
    back = decode_token_claims(tok)
    assert back["tenant_id"] == "t1"
    assert back["peer_id"] == "A"
    assert back["rendezvous"] == "rv"
    assert back["aud"] == "fantastic.relay"


def test_ttl_clamped_to_60s():
    claims = build_claims(
        tenant_id="t", peer_id="A", rendezvous="rv", ttl=999, now=1000
    )
    assert claims["exp"] - claims["iat"] == MAX_TTL == 60
    assert claims["nbf"] == claims["iat"] == 1000


def test_signed_token_signature_verifies():
    priv = Ed25519PrivateKey.generate()
    priv_bytes = priv.private_bytes(
        serialization.Encoding.Raw,
        serialization.PrivateFormat.Raw,
        serialization.NoEncryption(),
    )
    claims = build_claims(tenant_id="t", peer_id="A", rendezvous="rv", now=1000)
    tok = encode_signed_token(claims, priv_bytes)
    seg, sigseg = tok.split(".")
    priv.public_key().verify(
        b64url_decode(sigseg), b64url_decode(seg)
    )  # raises on bad sig


# ─── _tls cert ───────────────────────────────────────────────────


def test_self_signed_cert_is_deterministic_and_carries_the_pubkey():
    k = _ed()
    cert_a, key_a = self_signed_cert(k)
    cert_b, _ = self_signed_cert(k)
    assert cert_a == cert_b  # deterministic — stable to pin across reboots
    # the cert's public key IS the device identity key
    from cryptography import x509

    der = x509.load_pem_x509_certificate(cert_a).public_bytes(
        serialization.Encoding.DER
    )
    assert peer_pubkey_from_der(der) == _pub(k)


# ─── CloudBridgeTransport over a fake relay (TLS 1.3 mTLS) ───────

_WSCLOSED = object()


class FakeWS:
    """Mimics a `websockets` client whose peer is the relay: send/recv binary
    frames, cross-wired to the partner leg (the relay forwards verbatim)."""

    def __init__(self, out_q: asyncio.Queue, in_q: asyncio.Queue) -> None:
        self._out = out_q
        self._in = in_q
        self.closed = False

    @classmethod
    def pair(cls):
        q1: asyncio.Queue = asyncio.Queue()
        q2: asyncio.Queue = asyncio.Queue()
        return cls(q1, q2), cls(q2, q1)

    async def send(self, data) -> None:
        await self._out.put(data)

    async def recv(self):
        item = await self._in.get()
        if item is _WSCLOSED:
            raise ConnectionResetError("fake ws closed")
        return item

    async def close(self) -> None:
        self.closed = True
        try:
            self._out.put_nowait(_WSCLOSED)
        except Exception:
            pass


async def _connect_pair(monkeypatch, heartbeat=0.0, a_trusts=None, expect=False):
    import websockets

    ws_a, ws_b = FakeWS.pair()
    seq = [ws_a, ws_b]

    async def fake_connect(url, **kw):
        return seq.pop(0)

    monkeypatch.setattr(websockets, "connect", fake_connect)

    ka, kb = _ed(), _ed()
    ac, ak = self_signed_cert(ka)
    bc, bk = self_signed_cert(kb)
    a_trust = a_trusts if a_trusts is not None else [bc]

    res = await asyncio.gather(
        CloudBridgeTransport.connect(
            "wss://relay",
            "tokA",
            server=False,
            cert_pem=ac,
            key_pem=ak,
            approved_certs_pem=a_trust,
            expected_partner_pubkey=_pub(kb) if expect else None,
            heartbeat=heartbeat,
        ),
        CloudBridgeTransport.connect(
            "wss://relay",
            "tokB",
            server=True,
            cert_pem=bc,
            key_pem=bk,
            approved_certs_pem=[ac],
            expected_partner_pubkey=_pub(ka) if expect else None,
            heartbeat=heartbeat,
        ),
        return_exceptions=True,
    )
    return res, ka, kb


async def test_transport_round_trip_and_peer_identity(monkeypatch):
    (ta, tb), ka, kb = await _connect_pair(monkeypatch, expect=True)
    assert not isinstance(ta, Exception), ta
    assert not isinstance(tb, Exception), tb
    # each side learned the other's real Ed25519 identity from the pinned cert
    assert ta.peer_identity["pubkey"] == _pub(kb)
    assert tb.peer_identity["pubkey"] == _pub(ka)
    await ta.send(
        {"type": "call", "id": "A:1", "target": "x", "payload": {"type": "reflect"}}
    )
    assert await tb.recv() == {
        "type": "call",
        "id": "A:1",
        "target": "x",
        "payload": {"type": "reflect"},
    }
    await tb.send({"type": "reply", "id": "A:1", "data": {"ok": True}})
    assert await ta.recv() == {"type": "reply", "id": "A:1", "data": {"ok": True}}
    await ta.close()
    await tb.close()


async def test_transport_large_frame_spans_records(monkeypatch):
    (ta, tb), _, _ = await _connect_pair(monkeypatch)
    blob = "x" * 200_000  # > one TLS record → length-framing reassembles it
    await ta.send({"type": "reply", "id": "A:1", "data": {"blob": blob}})
    got = await tb.recv()
    assert got["data"]["blob"] == blob
    await ta.close()
    await tb.close()


async def test_transport_keepalive_is_dropped(monkeypatch):
    (ta, tb), _, _ = await _connect_pair(monkeypatch)
    await ta.send({"type": KEEPALIVE_TYPE})
    await ta.send({"type": "reply", "id": "A:1", "data": {"ok": True}})
    assert await tb.recv() == {"type": "reply", "id": "A:1", "data": {"ok": True}}
    await ta.close()
    await tb.close()


async def test_transport_pins_peer_cert_rejects_unapproved(monkeypatch):
    # A approves a THIRD party's pubkey, not B's → A fails the TLS handshake
    # (the verify callback rejects B's pubkey).
    wrong = self_signed_cert(_ed())[0]
    (res), _, _ = await _connect_pair(monkeypatch, a_trusts=[wrong])
    ta, tb = res
    from OpenSSL import SSL

    assert isinstance(ta, SSL.Error), ta  # pubkey pin mismatch → handshake fails
    assert isinstance(tb, Exception), tb  # peer torn down too


# ─── TokenSource seam (cloud_bridge obtains a token, never authenticates) ──


async def test_token_source_literal():
    assert await cb._resolve_token({"token": "LIT"}, kernel=None) == "LIT"


async def test_token_source_command_passes_context_via_env():
    rec = {
        "token_command": ["sh", "-c", "printf 'TOK-%s' \"$FANTASTIC_RELAY_PEER\""],
        "peer_id": "A",
    }
    assert await cb._resolve_token(rec, kernel=None) == "TOK-A"


async def test_token_source_issue_url_posts_and_returns_token():
    # A mock relay `/issue`: 200 token on the right credential, 401 otherwise. The
    # request body carries the provider + credential + rendezvous context.
    import http.server
    import json
    import threading

    seen = {}

    class _H(http.server.BaseHTTPRequestHandler):
        def do_POST(self):
            body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
            seen.update(body)
            if body.get("credential") != "hunter2":
                self.send_response(401)
                self.end_headers()
                return
            tok = f"TOK-{body['provider']}-{body['peer_id']}-{body['partner_peer_id']}-{body['rendezvous']}"
            self.send_response(200)
            self.send_header("Content-Type", "text/plain")
            self.end_headers()
            self.wfile.write(tok.encode())

        def log_message(self, *a):  # quiet
            pass

    srv = http.server.HTTPServer(("127.0.0.1", 0), _H)
    threading.Thread(target=srv.serve_forever, daemon=True).start()
    url = f"http://127.0.0.1:{srv.server_address[1]}/issue"
    try:
        rec = {
            "issue_url": url,
            "password": "hunter2",
            "provider": "password",
            "peer_id": "A",
            "partner_peer_id": "B",
            "rendezvous": "rv1",
        }
        assert await cb._resolve_token(rec, kernel=None) == "TOK-password-A-B-rv1"
        # the issuer never sees a token, only the credential + context.
        assert seen["credential"] == "hunter2" and "token" not in seen
        # wrong credential → denied (401 → ValueError, fail closed).
        rec["password"] = "wrong"
        with pytest.raises(ValueError):
            await cb._resolve_token(rec, kernel=None)
    finally:
        srv.shutdown()


class _FakeKernel:
    def __init__(self, token):
        self._token = token
        self.seen = None

    async def send(self, target, payload):
        self.seen = (target, payload)
        return {"token": self._token}


async def test_token_source_provider_agent():
    k = _FakeKernel("PTOK")
    rec = {
        "token_provider": "issuer",
        "peer_id": "A",
        "rendezvous": "rv",
        "partner_peer_id": "B",
    }
    assert await cb._resolve_token(rec, k) == "PTOK"
    assert k.seen[0] == "issuer"
    assert k.seen[1]["type"] == "mint_token"
    assert k.seen[1]["peer_id"] == "A"


async def test_token_source_dev_token_is_unsigned():
    rec = {"dev_token": True, "tenant_id": "t", "peer_id": "A", "rendezvous": "rv"}
    tok = await cb._resolve_token(rec, kernel=None)
    assert "." not in tok  # unsigned claims-only (ROUTER_REQUIRE_AUTH=false)
    assert decode_token_claims(tok)["peer_id"] == "A"


async def test_token_source_missing_errors():
    with pytest.raises(ValueError):
        await cb._resolve_token({}, kernel=None)


# ─── engine integration: forward through cloud_bridge.handler ────


@pytest.fixture
def two_kernels(tmp_path, monkeypatch):
    a_dir, b_dir = tmp_path / "a", tmp_path / "b"
    a_dir.mkdir()
    b_dir.mkdir()
    monkeypatch.chdir(a_dir)
    ka = boot_root()
    ka.ensure("cli", "cli.tools", display_name="cli")
    monkeypatch.chdir(b_dir)
    kb_kern = boot_root()
    kb_kern.ensure("cli", "cli.tools", display_name="cli")
    monkeypatch.chdir(tmp_path)
    yield ka, kb_kern
    cb._bridges.clear()
    cb._test_transport_inject.clear()


async def test_cloud_bridge_reuses_engine_forward(two_kernels):
    """A cloud_bridge agent (memory transport injected) forwards to kernel B's
    root reflect and the reply tunnels back — proving cloud_bridge.handler runs on
    the shared io_bridge engine, transport aside."""
    ka, kb_kern = two_kernels
    rec_a = await ka.send(
        "kernel_state",
        {
            "type": "create_agent",
            "handler_module": "cloud_bridge.tools",
            "transport": "memory",
            "peer_id": "PLACEHOLDER",
        },
    )
    a_id = rec_a["id"]
    rec_b = await kb_kern.send(
        "kernel_state",
        {
            "type": "create_agent",
            "handler_module": "cloud_bridge.tools",
            "transport": "memory",
            "peer_id": a_id,
            # B is the SERVING leg that dispatches the forwarded call; sealed by
            # default, so open it explicitly to permit inbound dispatch.
            "ingress_rule": "allow_all",
        },
    )
    b_id = rec_b["id"]

    mt_a, mt_b = MemoryTransport.pair()
    cb._test_transport_inject[a_id] = mt_a
    cb._test_transport_inject[b_id] = mt_b
    assert (await ka.send(a_id, {"type": "boot"})).get("booted") is True
    assert (await kb_kern.send(b_id, {"type": "boot"})).get("booted") is True

    r = await ka.send(
        a_id,
        {
            "type": "forward",
            "target": "kernel_state",
            "payload": {"type": "reflect"},
        },
    )
    assert isinstance(r, dict), r
    assert r["id"] == "kernel_state", r
    assert r["tree"]["id"] == "kernel_state"


async def test_cloud_bridge_reflect_fields(two_kernels):
    ka, _ = two_kernels
    rec = await ka.send(
        "kernel_state",
        {
            "type": "create_agent",
            "handler_module": "cloud_bridge.tools",
            "transport": "cloud_bridge",
            "relay_url": "wss://relay/x",
            "tenant_id": "t1",
            "peer_id": "A",
            "rendezvous": "rv",
            "partner_peer_id": "B",
        },
    )
    r = await ka.send(rec["id"], {"type": "reflect"})
    assert r["transport"] == "cloud_bridge"
    assert r["connected"] is False
    assert r["relay_url"] == "wss://relay/x"
    assert r["tenant_id"] == "t1"
    assert r["rendezvous"] == "rv"
    for v in (
        "boot",
        "forward",
        "watch_remote",
        "unwatch_remote",
        "reconnect",
        "reflect",
    ):
        assert v in r["verbs"]


async def test_cloud_leg_resolves_deny_inbound(two_kernels):
    """A cloud_bridge leg booted with `auth="deny_inbound"` inherits the shared
    io_bridge resolver — its engine state carries a DenyInbound authorizer (the
    cross-runtime gate, exercised end-to-end by the relay_e2e matrix)."""
    ka, _ = two_kernels
    rec = await ka.send(
        "kernel_state",
        {
            "type": "create_agent",
            "handler_module": "cloud_bridge.tools",
            "transport": "memory",
            "peer_id": "PLACEHOLDER",
            "auth": "deny_inbound",
        },
    )
    a_id = rec["id"]
    mt_a, _mt_b = MemoryTransport.pair()
    cb._test_transport_inject[a_id] = mt_a
    assert (await ka.send(a_id, {"type": "boot"})).get("booted") is True
    assert isinstance(core._state(a_id).ingress, DenyInbound)
    # reflect surfaces the active rule TYPE name back (here the explicit
    # `deny_inbound`; an un-ruled leg would also surface `deny_inbound` — sealed
    # by default).
    assert (await ka.send(a_id, {"type": "reflect"}))["ingress_rule"] == "deny_inbound"


async def test_cloud_leg_resolves_password(two_kernels):
    """A cloud_bridge leg booted with the object-form `auth={policy:password,...}`
    inherits the shared resolver — Password with the threaded `token_env`. reflect
    surfaces only the policy NAME (never the env-var config)."""
    ka, _ = two_kernels
    rec = await ka.send(
        "kernel_state",
        {
            "type": "create_agent",
            "handler_module": "cloud_bridge.tools",
            "transport": "memory",
            "peer_id": "PLACEHOLDER",
            "auth": {"policy": "password", "token_env": "FANTASTIC_GROUP_TOKEN"},
        },
    )
    a_id = rec["id"]
    mt_a, _mt_b = MemoryTransport.pair()
    cb._test_transport_inject[a_id] = mt_a
    assert (await ka.send(a_id, {"type": "boot"})).get("booted") is True
    ingress = core._state(a_id).ingress
    assert (
        isinstance(ingress, Password) and ingress.token_env == "FANTASTIC_GROUP_TOKEN"
    )
    r = await ka.send(a_id, {"type": "reflect"})
    assert r["ingress_rule"] == "password" and r["egress_rule"] == "password"
