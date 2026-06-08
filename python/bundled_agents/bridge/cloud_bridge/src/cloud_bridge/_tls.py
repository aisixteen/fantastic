"""End-to-end TLS 1.3 mutual auth for cloud_bridge (E2E decision 2026-06-07).

Replaces the prior hand-rolled Noise layer. TLS does the WHOLE security-critical
handshake — mutual authentication, forward secrecy, transcript integrity,
downgrade protection — via a vetted stack, driven over the relay's opaque byte
pipe with a memory BIO (no socket). We write NO crypto.

Identity = a self-signed **Ed25519** cert (RFC 8410/8422) whose key IS the device
`peer_id` identity — so the relay token's `peer_id` and the TLS cert are the same
key (no separate static, no key-binding gap). Peer-approval = **pinning the
durable identity = the Ed25519 PUBLIC KEY**, not the cert bytes: a custom verify
callback extracts the peer leaf's pubkey and checks it against the approved
device set, overriding openssl's "self-signed ⇒ untrusted" verdict. The cert is a
disposable carrier (it may rotate / be non-deterministic across runtimes — e.g.
Swift's CryptoKit randomizes Ed25519 signatures); only the key is the identity.

This needs a per-cert verify hook, which Python's stdlib `ssl` does NOT expose
(it can only trust exact CA certs), so the TLS layer here is **pyOpenSSL**
(`OpenSSL.SSL`) — same openssl under the hood, plus `Context.set_verify`. Cert
construction still uses `cryptography` (below).
"""

from __future__ import annotations

from datetime import datetime, timezone

# Fixed validity (the cert's TBS is otherwise constant from the key).
_NOT_BEFORE = datetime(2020, 1, 1, tzinfo=timezone.utc)
_NOT_AFTER = datetime(2050, 1, 1, tzinfo=timezone.utc)


def self_signed_cert(ed25519_priv: bytes) -> tuple[bytes, bytes]:
    """Deterministic self-signed Ed25519 cert (PEM) + PKCS8 key (PEM) for a device
    identity key. Same key → same cert bytes (pinning is stable across reboots)."""
    from cryptography import x509
    from cryptography.hazmat.primitives import serialization
    from cryptography.hazmat.primitives.asymmetric.ed25519 import Ed25519PrivateKey
    from cryptography.x509.oid import NameOID

    from cloud_bridge._token import b64url

    key = Ed25519PrivateKey.from_private_bytes(ed25519_priv)
    cn = b64url(key.public_key().public_bytes_raw())
    name = x509.Name([x509.NameAttribute(NameOID.COMMON_NAME, cn)])
    cert = (
        x509.CertificateBuilder()
        .subject_name(name)
        .issuer_name(name)
        .public_key(key.public_key())
        .serial_number(1)
        .not_valid_before(_NOT_BEFORE)
        .not_valid_after(_NOT_AFTER)
        # CA:TRUE so the self-signed leaf can act as its own trust anchor when pinned.
        .add_extension(x509.BasicConstraints(ca=True, path_length=None), critical=True)
        .sign(key, None)  # Ed25519 signs with algorithm=None
    )
    cert_pem = cert.public_bytes(serialization.Encoding.PEM)
    key_pem = key.private_bytes(
        serialization.Encoding.PEM,
        serialization.PrivateFormat.PKCS8,
        serialization.NoEncryption(),
    )
    return cert_pem, key_pem


def cert_pem_for(ed25519_priv: bytes) -> bytes:
    """Just the cert PEM (e.g. to publish into an account device list)."""
    return self_signed_cert(ed25519_priv)[0]


def peer_pubkey_from_der(der: bytes) -> bytes:
    """The raw Ed25519 public key of a peer's DER cert (its durable `peer_id`)."""
    from cryptography import x509

    return x509.load_der_x509_certificate(der).public_key().public_bytes_raw()


def pubkey_from_pem(pem: bytes) -> bytes:
    """The raw Ed25519 public key carried by a cert PEM (the approved identity)."""
    from cryptography import x509

    return x509.load_pem_x509_certificate(pem).public_key().public_bytes_raw()


def make_context(
    *,
    server: bool,
    cert_pem: bytes,
    key_pem: bytes,
    approved_certs_pem: list[bytes],
):
    """A TLS 1.3-only, mutually-authenticated `OpenSSL.SSL.Context` that pins the
    peer by its Ed25519 PUBLIC KEY (∈ the approved device set) via a custom verify
    callback — so a non-deterministic / rotated peer cert with the same key still
    validates. Raises if no approved certs are given (fail closed)."""
    from cryptography import x509
    from cryptography.hazmat.primitives.serialization import load_pem_private_key
    from OpenSSL import SSL

    if not approved_certs_pem:
        raise ValueError(
            "cloud_bridge TLS: no approved_peer_certs to pin (fail closed)"
        )

    # Approved DEVICE IDENTITIES = the pubkeys carried by the approved certs.
    approved: set[bytes] = {pubkey_from_pem(p) for p in approved_certs_pem}

    ctx = SSL.Context(SSL.TLS_SERVER_METHOD if server else SSL.TLS_CLIENT_METHOD)
    ctx.set_min_proto_version(SSL.TLS1_3_VERSION)
    ctx.set_max_proto_version(SSL.TLS1_3_VERSION)
    # The min/max pin above already forces TLS 1.3, but also disable every
    # pre-1.3 protocol explicitly via OP_NO_*: static analysis (CodeQL
    # py/insecure-protocol) only recognizes these options, so this both clears
    # the alert and makes "TLS 1.3 only" unmistakable / defence-in-depth.
    ctx.set_options(
        SSL.OP_NO_SSLv2
        | SSL.OP_NO_SSLv3
        | SSL.OP_NO_TLSv1
        | SSL.OP_NO_TLSv1_1
        | SSL.OP_NO_TLSv1_2
    )
    ctx.use_certificate(x509.load_pem_x509_certificate(cert_pem))
    ctx.use_privatekey(load_pem_private_key(key_pem, password=None))

    def _verify(_conn, x509cert, _errno, depth, _ok) -> bool:
        # Pin the LEAF (depth 0) by its Ed25519 pubkey ∈ approved, overriding
        # openssl's chain verdict (a self-signed leaf is "untrusted" by default).
        if depth != 0:
            return True
        try:
            return (
                x509cert.to_cryptography().public_key().public_bytes_raw() in approved
            )
        except Exception:
            return False

    # VERIFY_PEER both requests the peer cert (mutual auth on the server) and runs
    # `_verify`; FAIL_IF_NO_PEER_CERT closes the door on an anonymous client.
    ctx.set_verify(SSL.VERIFY_PEER | SSL.VERIFY_FAIL_IF_NO_PEER_CERT, _verify)
    return ctx
