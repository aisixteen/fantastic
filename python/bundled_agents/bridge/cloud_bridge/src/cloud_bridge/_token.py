"""Relay auth tokens (CONTRACT v1).

The relay authenticates a leg at the WS handshake via a subprotocol value:

    Sec-WebSocket-Protocol: fantastic.relay.v1, <base64url-nopad(token)>

where token is either:
  - **signed**  `<b64url(claims_json)>.<b64url(ed25519_sig)>` — the signature is a
    *detached* Ed25519 over the raw `claims_json` bytes, verifiable with the
    control plane's published key (production), or
  - **dev**     `<b64url(claims_json)>` — claims only, no signature (the relay's
    `ROUTER_REQUIRE_AUTH=false` dev posture; `relay-probe` uses this).

**cloud_bridge does NOT mint tokens in production** — it receives them from a
host-provided token-provider seam (see `tools.py`). These builders exist for the
self-hosted control-plane path and for tests. Claims/field names + the
`exp - iat <= 60s` rule are frozen by `../fantastic_relay/CONTRACT.md`.
"""

from __future__ import annotations

import base64
import json
import os
import time
from typing import Any

AUD = "fantastic.relay"
MAX_TTL = 60  # CONTRACT: exp - iat <= 60s (strict mode)


def b64url(data: bytes) -> str:
    """base64url, no padding (CONTRACT wire encoding)."""
    return base64.urlsafe_b64encode(data).rstrip(b"=").decode("ascii")


def b64url_decode(s: str) -> bytes:
    pad = "=" * (-len(s) % 4)
    return base64.urlsafe_b64decode(s + pad)


def build_claims(
    *,
    tenant_id: str,
    peer_id: str,
    rendezvous: str,
    partner_peer_id: str = "",
    aud: str = AUD,
    ttl: int = MAX_TTL,
    now: float | None = None,
) -> dict[str, Any]:
    """A CONTRACT-v1 claims dict. `ttl` is clamped to MAX_TTL (the relay rejects
    `exp - iat > 60s`). `jti` is a fresh random id (single-use within validity)."""
    t = int(now if now is not None else time.time())
    ttl = min(int(ttl), MAX_TTL)
    return {
        "tenant_id": tenant_id,
        "peer_id": peer_id,
        "rendezvous": rendezvous,
        "partner_peer_id": partner_peer_id,
        "aud": aud,
        "iat": t,
        "nbf": t,
        "exp": t + ttl,
        "jti": b64url(os.urandom(12)),
    }


def _claims_bytes(claims: dict[str, Any]) -> bytes:
    # Compact, deterministic separators so the signed bytes are stable.
    return json.dumps(claims, separators=(",", ":"), sort_keys=True).encode("utf-8")


def encode_dev_token(claims: dict[str, Any]) -> str:
    """Dev token (claims only, unsigned). Relay must run `ROUTER_REQUIRE_AUTH=false`."""
    return b64url(_claims_bytes(claims))


def encode_signed_token(claims: dict[str, Any], ed25519_priv: bytes) -> str:
    """Production token: `<b64url(claims)>.<b64url(detached ed25519 sig)>`.
    `ed25519_priv` is the control plane's 32-byte raw private key."""
    from cryptography.hazmat.primitives.asymmetric.ed25519 import Ed25519PrivateKey

    raw = _claims_bytes(claims)
    sig = Ed25519PrivateKey.from_private_bytes(ed25519_priv).sign(raw)
    return f"{b64url(raw)}.{b64url(sig)}"


def decode_token_claims(token: str) -> dict[str, Any]:
    """Parse the claims segment of a (dev or signed) token. Does NOT verify the
    signature — verification is the relay's job; cloud_bridge only needs the
    claims to learn its own `rendezvous` / `partner_peer_id` when echoing them."""
    seg = token.split(".", 1)[0]
    return json.loads(b64url_decode(seg))
