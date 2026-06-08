"""Harness for the kernels-through-relay e2e (`integration_tests/relay_e2e/`).

Builds nothing — it LOCATES + boots the relay binaries from the sibling repo
(`../fantastic_relay/…`, see `tmp/relay_e2e_setup.md`) and generates device
identities. It also serves a `POST /issue` control-plane endpoint (`Relay.issue_url`)
— a stand-in for the relay's, backed by the SAME `fantastic-issue` minter — so the
e2e exercises the kernels' production `issue_url` TokenSource (POST a credential →
get a token) rather than a pre-minted literal. Skips cleanly (pytest.skip) when a
binary isn't built, matching the integration-test convention.

Device-identity carriers (`cloud_cert`) come from each runtime's OWN cert builder;
peers pin by Ed25519 PUBLIC KEY, so only the key need match (the presented cert may
differ — swift's is non-deterministic).
"""

from __future__ import annotations

import base64
import http.server
import json
import os
import socket
import subprocess
import threading
import time
from pathlib import Path

import pytest

# integration_tests/relay_e2e/ → repo root is two up.
_REPO_ROOT = Path(__file__).resolve().parents[2]
_RELAY = _REPO_ROOT.parent / "fantastic_relay" / "rust" / "target" / "release"
_CANVAS_PY = _REPO_ROOT / "python" / ".venv" / "bin" / "python"

ROUTER_BIN = _RELAY / "fantastic-router"
ISSUE_BIN = _RELAY / "fantastic-issue"
PASSWORD = "hunter2"  # control-plane password (issuer-side only; relay never sees it)


def require_relay() -> tuple[Path, Path]:
    """The (router, issuer) binary paths, or skip if either isn't built."""
    if not ROUTER_BIN.exists() or not ISSUE_BIN.exists():
        pytest.skip(
            "relay binaries not built — run:\n"
            "  cargo build --release --manifest-path ../fantastic_relay/rust/Cargo.toml"
        )
    if not _CANVAS_PY.exists():
        pytest.skip("canvas python venv missing — run `cd python && uv sync`")
    return ROUTER_BIN, ISSUE_BIN


def _b64url(b: bytes) -> str:
    return base64.urlsafe_b64encode(b).rstrip(b"=").decode("ascii")


def new_id_key() -> str:
    """A fresh device identity key (b64url-nopad); any 32 bytes is a valid Ed25519 seed."""
    return _b64url(os.urandom(32))


def _python_cert(id_key: str) -> bytes:
    """The PEM the PYTHON kernel presents for `id_key` — built by the canvas venv's
    real `cloud_bridge._tls.self_signed_cert`, so it byte-matches the kernel."""
    script = (
        "import os,sys;"
        "from cloud_bridge._tls import self_signed_cert;"
        "from cloud_bridge._token import b64url_decode;"
        "c,_=self_signed_cert(b64url_decode(os.environ['IDK']));"
        "sys.stdout.buffer.write(c)"
    )
    out = subprocess.run(
        [str(_CANVAS_PY), "-c", script],
        env={**os.environ, "IDK": id_key},
        capture_output=True,
        cwd=str(_REPO_ROOT / "python"),
    )
    if out.returncode != 0:
        raise RuntimeError(f"python cert gen failed: {out.stderr.decode('utf-8', 'replace')}")
    return out.stdout


def cloud_cert(runtime: str, id_key: str, launcher, workdir: Path) -> bytes:
    """A cert (PEM bytes) carrying `id_key`'s Ed25519 pubkey — the device identity
    the peer PINS. Peers pin by pubkey, so any cert with the right key works; we
    derive each runtime's OWN cert (python via its `_tls` builder; rust/swift via
    the kernel binary's `__cloud-cert` subcommand) so the test mirrors production —
    the presented cert may differ from this carrier (swift's is non-deterministic)
    yet the pubkey matches."""
    if runtime == "python":
        return _python_cert(id_key)
    out = launcher.cli(workdir, ["__cloud-cert", id_key])
    if out.returncode != 0 or not out.stdout.strip():
        raise RuntimeError(f"{runtime} __cloud-cert failed: rc={out.returncode} {out.stderr!r}")
    return out.stdout.encode("ascii")


class Relay:
    """A booted `fantastic-router` on loopback + its control-plane signing key
    (for minting tokens). Use as a context manager / fixture."""

    def __init__(self, router_bin: Path, issue_bin: Path, port: int) -> None:
        self.router_bin = router_bin
        self.issue_bin = issue_bin
        self.port = port
        self.url = f"ws://127.0.0.1:{port}/"
        self._proc: subprocess.Popen | None = None
        self._signing_key = ""
        self._pubkey = ""
        self._issue_srv: http.server.HTTPServer | None = None
        self._issue_url = ""

    @property
    def issue_url(self) -> str:
        """The control-plane `POST /issue` endpoint (the cloud_bridge `issue_url`
        TokenSource). The relay router itself doesn't yet serve HTTP /issue, so the
        harness runs a faithful stand-in backed by the SAME token minter the relay
        uses (`fantastic-issue` / `issuer.rs`): it authenticates `provider`/
        `credential` and mints a real signed token. Body + semantics match the
        canvas spec; when the relay ships its own /issue, point this at it instead."""
        return self._issue_url

    def _keygen(self) -> None:
        out = subprocess.run([str(self.issue_bin), "keygen"], capture_output=True, text=True)
        for line in out.stdout.splitlines():
            if line.startswith("RELAY_SIGNING_KEY="):
                self._signing_key = line.split("=", 1)[1].strip()
            elif line.startswith("ROUTER_CONTROL_PLANE_PUBKEY="):
                self._pubkey = line.split("=", 1)[1].strip()
        if not (self._signing_key and self._pubkey):
            raise RuntimeError(f"keygen produced no keys: {out.stdout!r} {out.stderr!r}")

    def start(self) -> "Relay":
        self._keygen()
        env = {
            **os.environ,
            "ROUTER_CONTROL_PLANE_PUBKEY": self._pubkey,
            "ROUTER_LISTEN_ADDR": f"127.0.0.1:{self.port}",
        }
        self._proc = subprocess.Popen(
            [str(self.router_bin)],
            env=env,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )
        # Wait for the listener to accept.
        deadline = time.time() + 10.0
        while time.time() < deadline:
            if self._proc.poll() is not None:
                raise RuntimeError(f"relay exited early (code {self._proc.returncode})")
            with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as s:
                s.settimeout(0.2)
                try:
                    s.connect(("127.0.0.1", self.port))
                    self._start_issue_server()
                    return self
                except OSError:
                    time.sleep(0.1)
        raise TimeoutError(f"relay did not listen on {self.port}")

    def _start_issue_server(self) -> None:
        """Stand up the `/issue` control-plane endpoint (stand-in for the relay's,
        backed by the real `fantastic-issue` minter). POST body
        `{provider, credential, peer_id, partner_peer_id, rendezvous}` →
        200 token (text/plain) | 401 denied."""
        relay = self

        class _Issue(http.server.BaseHTTPRequestHandler):
            def do_POST(self):  # noqa: N802
                if self.path != "/issue":
                    self.send_response(404)
                    self.end_headers()
                    return
                try:
                    body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
                except Exception:
                    self.send_response(400)
                    self.end_headers()
                    return
                # Authenticate the credential (the relay's PasswordProvider posture).
                if (
                    body.get("provider", "password") != "password"
                    or body.get("credential") != PASSWORD
                ):
                    self.send_response(401)
                    self.end_headers()
                    return
                try:
                    tok = relay.token(
                        peer=body.get("peer_id") or "",
                        partner=body.get("partner_peer_id") or "",
                        rendezvous=body.get("rendezvous") or "",
                    )
                except Exception as e:  # pragma: no cover - mint failure
                    self.send_response(500)
                    self.end_headers()
                    self.wfile.write(str(e).encode("utf-8"))
                    return
                self.send_response(200)
                self.send_header("Content-Type", "text/plain")
                self.end_headers()
                self.wfile.write(tok.encode("ascii"))

            def log_message(self, *a):  # quiet
                pass

        self._issue_srv = http.server.HTTPServer(("127.0.0.1", 0), _Issue)
        self._issue_url = f"http://127.0.0.1:{self._issue_srv.server_address[1]}/issue"
        threading.Thread(target=self._issue_srv.serve_forever, daemon=True).start()

    def token(self, *, peer: str, partner: str, rendezvous: str) -> str:
        """Mint a relay token for one leg (peer reaching partner over rendezvous)."""
        out = subprocess.run(
            [
                str(self.issue_bin),
                "token",
                "--password",
                PASSWORD,
                "--peer",
                peer,
                "--partner",
                partner,
                "--rendezvous",
                rendezvous,
            ],
            env={**os.environ, "RELAY_SIGNING_KEY": self._signing_key, "RELAY_PASSWORD": PASSWORD},
            capture_output=True,
            text=True,
        )
        if out.returncode != 0:
            raise RuntimeError(f"token mint failed: {out.stderr!r}")
        token = out.stdout.strip()
        if not token:
            raise RuntimeError("token mint produced empty output")
        return token

    def stop(self) -> None:
        if self._issue_srv is not None:
            self._issue_srv.shutdown()
            self._issue_srv = None
        if self._proc is not None and self._proc.poll() is None:
            self._proc.terminate()
            try:
                self._proc.wait(timeout=3.0)
            except subprocess.TimeoutExpired:
                self._proc.kill()
        self._proc = None
