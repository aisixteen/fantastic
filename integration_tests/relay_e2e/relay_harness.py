"""Harness for the relay_connector e2e (`integration_tests/relay_e2e/`).

Boots the relay-KERNEL (`relayd`, sibling repo `../fantastic_relay/relaykernel`)
and skips cleanly (pytest.skip) when it isn't available — matching the
integration-test convention. Two targets, like the rest of the suite:

  - `FANTASTIC_TARGET=local` (default): a local `relayd` subprocess on loopback;
    kernels dial `ws://127.0.0.1:<port>`.
  - `FANTASTIC_TARGET=container`: the `relay:latest` image, published on the host;
    the kernel CONTAINERS reach it via the host gateway
    `ws://host.containers.internal:<port>` (the cross-container "unit" model).

The relay is a kernel router: a connector dials `ws://<host>/<guid>` with the
group password in `X-Fantastic-Auth` (checked once at the WS upgrade), and the
relay routes by `target`. No certs / token-minting / issue-server — just the
connection password (`RELAY_PASSWORD`), set on the relay as `FANTASTIC_GROUP_TOKEN`.
"""

from __future__ import annotations

import os
import socket
import subprocess
import time
from pathlib import Path

import pytest

from helpers.launcher import CONTAINER_PEER_HOST, resolve_engine

# integration_tests/relay_e2e/ → canvas repo root is two up; the relay is a sibling.
_REPO_ROOT = Path(__file__).resolve().parents[2]
_RELAY_ROOT = _REPO_ROOT.parent / "fantastic_relay" / "relaykernel"

_TARGET = os.environ.get("FANTASTIC_TARGET", "local").strip().lower()
_RELAY_IMAGE = os.environ.get("RELAY_IMAGE", "relay:latest")
_RELAY_CONTAINER_PORT = 9443  # the relay's in-image listen port (entrypoint default).

# The relay's CONNECTION group password (the X-Fantastic-Auth a connector must
# present at the WS upgrade). Distinct from a daemon's FANTASTIC_GROUP_TOKEN,
# which is the io_bridge `password` rule's group secret for the tunneled calls.
RELAY_PASSWORD = "hunter2"


def _relayd() -> Path | None:
    """Newest built `relayd` (release preferred by mtime), or None."""
    candidates = [
        _RELAY_ROOT / ".build" / "release" / "relayd",
        _RELAY_ROOT / ".build" / "debug" / "relayd",
    ]
    existing = [c for c in candidates if c.exists()]
    return max(existing, key=lambda p: p.stat().st_mtime) if existing else None


def require_relay() -> Path | None:
    """The `relayd` binary path (local target), or None (container target). Skips
    if the relay isn't available for the active target."""
    if _TARGET == "container":
        engine = resolve_engine()
        if engine is None:
            pytest.skip("FANTASTIC_TARGET=container but no podman/docker found")
        if (
            subprocess.run(
                [engine, "image", "inspect", _RELAY_IMAGE], capture_output=True
            ).returncode
            != 0
        ):
            pytest.skip(
                f"relay image {_RELAY_IMAGE!r} not built — run:\n"
                "  sh ../fantastic_relay/relaykernel/container/build.sh"
            )
        return None
    binary = _relayd()
    if binary is None:
        pytest.skip("relayd not built — run:\n  cd ../fantastic_relay/relaykernel && swift build")
    return binary


def _wait_port(port: int, deadline_s: float, proc: subprocess.Popen | None) -> None:
    deadline = time.time() + deadline_s
    while time.time() < deadline:
        if proc is not None and proc.poll() is not None:
            raise RuntimeError(f"relayd exited early (code {proc.returncode})")
        with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as s:
            s.settimeout(0.3)
            try:
                s.connect(("127.0.0.1", port))
                return
            except OSError:
                time.sleep(0.15)
    raise TimeoutError(f"relay did not listen on {port}")


class Relay:
    """A booted relay on loopback (local subprocess) or as a published container.
    `url` is what a CONNECTOR dials: `ws://127.0.0.1:<port>` locally, or
    `ws://host.containers.internal:<port>` under the container target (so kernel
    containers reach it through the host gateway)."""

    def __init__(self, relayd: Path | None, port: int, password: str = RELAY_PASSWORD) -> None:
        self.relayd = relayd
        self.port = port
        self.password = password
        host = CONTAINER_PEER_HOST if _TARGET == "container" else "127.0.0.1"
        self.url = f"ws://{host}:{port}"
        self._proc: subprocess.Popen | None = None
        self._container: str | None = None

    def start(self) -> "Relay":
        if _TARGET == "container":
            return self._start_container()
        env = {
            **os.environ,
            "RELAY_LISTEN_ADDR": f"127.0.0.1:{self.port}",
            "FANTASTIC_GROUP_TOKEN": self.password,
            "RELAY_INGRESS_RULE": "password",
        }
        self._proc = subprocess.Popen(
            [str(self.relayd)], env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL
        )
        _wait_port(self.port, 10.0, self._proc)
        return self

    def _start_container(self) -> "Relay":
        engine = resolve_engine()
        name = f"ftit-relay-{self.port}-{os.getpid()}"
        subprocess.run([engine, "rm", "-f", name], capture_output=True)
        # Publish on ALL interfaces (`-p port:9443`) so the host gateway forwards
        # peer kernel containers to it; the host still reaches 127.0.0.1:port.
        cmd = [
            engine,
            "run",
            "-d",
            "--name",
            name,
            "-p",
            f"{self.port}:{_RELAY_CONTAINER_PORT}",
            "-e",
            f"FANTASTIC_GROUP_TOKEN={self.password}",
            "-e",
            f"RELAY_PORT={_RELAY_CONTAINER_PORT}",
            _RELAY_IMAGE,
        ]
        r = subprocess.run(cmd, capture_output=True, text=True)
        if r.returncode != 0:
            raise RuntimeError(f"relay container start failed: {r.stderr or r.stdout}")
        self._container = name
        _wait_port(self.port, 15.0, None)
        return self

    def stop(self) -> None:
        if self._container is not None:
            engine = resolve_engine()
            if engine is not None:
                subprocess.run([engine, "rm", "-f", self._container], capture_output=True)
            self._container = None
        if self._proc is not None and self._proc.poll() is None:
            self._proc.terminate()
            try:
                self._proc.wait(timeout=3.0)
            except subprocess.TimeoutExpired:
                self._proc.kill()
        self._proc = None
