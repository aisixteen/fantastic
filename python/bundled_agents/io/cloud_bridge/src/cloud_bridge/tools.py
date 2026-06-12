"""cloud_bridge — cross-kernel comms through a zero-trust relay (CONTRACT v1).

Same bridge verbs + wire frames as `ws_bridge` (it reuses the
`io_bridge` engine), but a different transport: instead of dialling a remote
`web_ws`, BOTH peers dial OUT (WSS) to a **relay** that authenticates each leg by a
control-plane token, **pairs** them by `(tenant_id, rendezvous)`, and forwards
**opaque** frames. The peers then run a mutually-authenticated **TLS 1.3** handshake
over that opaque pipe (vetted stdlib `ssl`, no hand-rolled crypto) and tunnel the
`call`/`reply`/`event` frames as TLS application data — the relay sees only
ciphertext, and a forged route fails the TLS handshake (impersonation impossible).

`transport = "cloud_bridge"`. Record fields:
  - `relay_url`        wss URL of the relay router (required)
  - `tenant_id`, `peer_id`, `rendezvous`  (required) + `partner_peer_id` (optional, binds the pair at the relay)
  - `id_key`           b64url Ed25519 private key — the durable device identity; its
                       self-signed cert IS the TLS identity (no separate key) (required)
  - `approved_peer_certs`  list of PEM device certs to PIN (the account device list) (required)
  - TokenSource (one of): `token` (literal) | `token_provider` (agent id answering
                    `mint_token`) | `issue_url` (+ `password`/`provider`: POST the
                    relay's `/issue` endpoint — native, provider-agnostic) |
                    `token_command` (subprocess printing a token — wraps the relay's
                    `fantastic-issue token` CLI for headless/e2e) |
                    `dev_token=true` (unsigned token for relay ROUTER_REQUIRE_AUTH=false)
  - role (one of): `tls_role` ("client"|"server") | `initiator` bool | derived
                    `peer_id < partner_peer_id` (initiator ⇒ TLS client)
  - `partner_pubkey`   optional b64url Ed25519 pubkey — assert the peer cert matches it
  - `ingress_rule` / `egress_rule`  optional, symmetric per-direction auth rules
                       (`auth` is a shorthand setting both). Each is `{"type": <name>,
                       "env": <var>}` (or a bare string type). Types:
                       `allow_all` (open the leg, full duplex) / `silent` (default
                       egress — present nothing) | `deny_inbound` (the SEAL and the
                       default ingress — refuse inbound `call`s, reply
                       `{reason:"unauthorized"}`) | `password`
                       (kernel-GROUP shared secret: ingress CHECKS the envelope
                       `auth_token` against the group token from `env`, default
                       `FANTASTIC_GROUP_TOKEN`; egress PRESENTS it). The token rides
                       the frame ENVELOPE — the dispatched payload stays clean.
                       Resolved by name from the `ingress_rules` / `egress_rules`
                       registries. ENFORCED ON THE RECEIVER. Distinct from the TLS
                       auth above — authorization, not authentication.
  - `heartbeat`        seconds between keepalives (default 30)

cloud_bridge does NOT authenticate or mint production tokens — it obtains one from a
TokenSource (`token`/`token_provider`/`token_command`) and presents it; the auth
method (password / Apple / Google) is invisible here. `dev_token` is the relay's
`ROUTER_REQUIRE_AUTH=false` dev posture only.

IO legs SEAL by default (absent ingress rule ⇒ `deny_inbound`; egress defaults to
`silent`). Securing is the default and OPENING a leg is the conscious control-plane
act, not this bundle's: an app exposing a leg sets its `ingress_rule`/`egress_rule`
(or `auth`) explicitly — e.g. `ingress_rule=allow_all` (open) or `=password` (group).
"""

from __future__ import annotations

from typing import Any

from io_bridge import make_verbs, dispatch
from io_bridge import on_delete as _engine_on_delete
from io_bridge import _BridgeState, _bridges, _test_transport_inject  # noqa: F401
from cloud_bridge import _tls, _token
from cloud_bridge._transport import CloudBridgeTransport

SENTENCE = "Cross-kernel comms through a zero-trust relay — dial-out, opaque frames, peer↔peer TLS 1.3 E2E."


async def _token_from_command(cmd, rec: dict) -> str:
    """Run a TokenSource subprocess (e.g. the relay repo's `fantastic-issue token`)
    and read the token from stdout. The rendezvous context is exported in the env
    (FANTASTIC_RELAY_{TENANT,PEER,PARTNER,RENDEZVOUS}) so a wrapper can use it; the
    auth credential (password / signing key) lives in the COMMAND's own env, never
    here — cloud_bridge does not authenticate."""
    import asyncio
    import os

    env = dict(os.environ)
    env.update(
        {
            "FANTASTIC_RELAY_TENANT": str(rec.get("tenant_id") or ""),
            "FANTASTIC_RELAY_PEER": str(rec.get("peer_id") or ""),
            "FANTASTIC_RELAY_PARTNER": str(rec.get("partner_peer_id") or ""),
            "FANTASTIC_RELAY_RENDEZVOUS": str(rec.get("rendezvous") or ""),
        }
    )
    if isinstance(cmd, str):
        proc = await asyncio.create_subprocess_shell(
            cmd, stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.PIPE, env=env
        )
    else:
        proc = await asyncio.create_subprocess_exec(
            *cmd,
            stdout=asyncio.subprocess.PIPE,
            stderr=asyncio.subprocess.PIPE,
            env=env,
        )
    out, err = await proc.communicate()
    if proc.returncode != 0:
        raise ValueError(
            f"token_command failed (exit {proc.returncode}): "
            f"{err.decode('utf-8', 'replace')[:200]}"
        )
    token = out.decode("utf-8").strip()
    if not token:
        raise ValueError("token_command produced no token")
    return token


async def _token_from_issue(rec: dict) -> str:
    """POST the relay's control-plane `/issue` endpoint and return the token.

    Body: `{provider, credential, peer_id, partner_peer_id, rendezvous}` →
    `200` token (text/plain), `401` denied. **Provider-agnostic**: the auth method
    lives in `provider` (`"password"` today; Apple / Google later = the SAME call,
    a different provider + credential). cloud_bridge still does not authenticate —
    it just relays the credential to the issuer and presents the returned token."""
    import asyncio
    import json as _json
    import urllib.error
    import urllib.request

    body = _json.dumps(
        {
            "provider": rec.get("provider") or "password",
            "credential": rec.get("password") or "",
            "peer_id": rec.get("peer_id"),
            "partner_peer_id": rec.get("partner_peer_id") or "",
            "rendezvous": rec.get("rendezvous"),
        }
    ).encode("utf-8")
    url = rec["issue_url"]

    def _post() -> str:
        req = urllib.request.Request(
            url, data=body, method="POST", headers={"Content-Type": "application/json"}
        )
        try:
            with urllib.request.urlopen(req, timeout=10.0) as resp:
                return resp.read().decode("utf-8").strip()
        except urllib.error.HTTPError as e:
            raise ValueError(f"issue endpoint denied (HTTP {e.code})") from e

    token = await asyncio.to_thread(_post)
    if not token:
        raise ValueError("issue endpoint returned no token")
    return token


async def _resolve_token(rec: dict, kernel: Any) -> str:
    """The TokenSource seam — cloud_bridge does NOT authenticate or mint; it just
    obtains a control-plane token (the auth method — password / Apple / Google — is
    invisible here) and presents it. Sources, in priority order:
      - `token`          a literal token the host already holds;
      - `token_provider` an in-kernel issuer agent answering `mint_token` (prod);
      - `issue_url`      POST the relay's `/issue` endpoint with `provider`/`password`
                         (native HTTP control-plane call; provider-agnostic);
      - `token_command`  a subprocess that prints a token (headless/e2e — wraps the
                         relay's `fantastic-issue token` CLI);
      - `dev_token=true` an unsigned claims-only token for the relay's
                         `ROUTER_REQUIRE_AUTH=false` dev posture (NOT production)."""
    token = rec.get("token")
    if token:
        return token
    provider = rec.get("token_provider")
    if provider:
        resp = await kernel.send(
            provider,
            {
                "type": "mint_token",
                "tenant_id": rec.get("tenant_id"),
                "peer_id": rec.get("peer_id"),
                "rendezvous": rec.get("rendezvous"),
                "partner_peer_id": rec.get("partner_peer_id") or "",
            },
        )
        token = (resp or {}).get("token")
        if token:
            return token
        raise ValueError(f"token_provider {provider!r} returned no token")
    if rec.get("issue_url"):
        return await _token_from_issue(rec)
    cmd = rec.get("token_command")
    if cmd:
        return await _token_from_command(cmd, rec)
    if rec.get("dev_token"):
        return _token.encode_dev_token(
            _token.build_claims(
                tenant_id=rec.get("tenant_id"),
                peer_id=rec.get("peer_id"),
                rendezvous=rec.get("rendezvous"),
                partner_peer_id=rec.get("partner_peer_id") or "",
            )
        )
    raise ValueError(
        "cloud_bridge requires a TokenSource: token, token_provider, token_command, or dev_token=true"
    )


def _derive_role(rec: dict, peer_id: str, partner: str) -> bool:
    """Return server=True/False. One peer is the TLS server, the other the client."""
    role = rec.get("tls_role")
    if role in ("client", "server"):
        return role == "server"
    initiator = rec.get("initiator")
    if initiator is None:
        if not partner:
            raise ValueError(
                "cloud_bridge needs tls_role, initiator, or partner_peer_id to pick a TLS role"
            )
        initiator = peer_id < partner  # deterministic; both ends agree
    return not bool(initiator)  # the initiator is the TLS client


async def build_transport(
    kind: str, rec: dict, kernel: Any, st: _BridgeState
) -> CloudBridgeTransport:
    """Build a `cloud_bridge` relay transport (TLS 1.3 mTLS) from the agent record."""
    if kind != "cloud_bridge":
        raise ValueError(f"cloud_bridge: unknown transport {kind!r}")

    relay_url = rec.get("relay_url")
    peer_id = rec.get("peer_id")
    rendezvous = rec.get("rendezvous")
    tenant_id = rec.get("tenant_id")
    if not (relay_url and peer_id and rendezvous and tenant_id):
        raise ValueError(
            "cloud_bridge requires relay_url, peer_id, rendezvous, tenant_id"
        )
    partner = rec.get("partner_peer_id") or ""

    id_key = rec.get("id_key")
    if not id_key:
        raise ValueError("cloud_bridge requires id_key (b64url ed25519 private key)")
    cert_pem, key_pem = _tls.self_signed_cert(_token.b64url_decode(id_key))

    approved = rec.get("approved_peer_certs")
    if not approved:
        raise ValueError(
            "cloud_bridge requires approved_peer_certs (pinned device list)"
        )
    approved_pem = [a.encode("ascii") if isinstance(a, str) else a for a in approved]

    server = _derive_role(rec, peer_id, partner)
    expected_pub = None
    if rec.get("partner_pubkey"):
        expected_pub = _token.b64url_decode(rec["partner_pubkey"])

    token = await _resolve_token(rec, kernel)
    heartbeat = float(rec.get("heartbeat", 30.0))

    transport = await CloudBridgeTransport.connect(
        relay_url,
        token,
        server=server,
        cert_pem=cert_pem,
        key_pem=key_pem,
        approved_certs_pem=approved_pem,
        expected_partner_pubkey=expected_pub,
        heartbeat=heartbeat,
    )
    pub = transport.peer_identity.get("pubkey")
    if pub:
        st.extra["verified_partner"] = _token.b64url(pub)
    return transport


def reflect_fields(rec: dict, st: _BridgeState) -> dict:
    """Relay-flavored reflect fields (no secrets)."""
    return {
        "relay_url": rec.get("relay_url"),
        "tenant_id": rec.get("tenant_id"),
        "peer_id": rec.get("peer_id"),
        "rendezvous": rec.get("rendezvous"),
        "partner_peer_id": rec.get("partner_peer_id"),
        "verified_partner": st.extra.get("verified_partner"),
    }


VERBS = make_verbs(
    build_transport=build_transport,
    sentence=SENTENCE,
    reflect_fields=reflect_fields,
    default_kind="cloud_bridge",
)


async def handler(id: str, payload: dict, kernel) -> dict | None:
    return await dispatch(VERBS, id, payload, kernel)


async def on_delete(agent):
    """Cascade hook — delegated to the shared engine (cancels read loop, closes the
    relay socket + heartbeat, rejects pending)."""
    return await _engine_on_delete(agent)
