"""kernel_bridge transport seam — WS (+ ssh-tunnelled WS) to a remote `web_ws`.

The bridge is an **asymmetric pure client**: it dials a remote kernel's
`web_ws` surface at `ws://{host}:{port}/{peer_id}/ws` and ships raw kernel-bridge
frames (`call`/`reply`/`watch`/`event`). The dispatch engine + verbs live in
`bridge_core`; this module supplies only the `ws` / `ssh+ws` transport builder,
the SSH tunnel helpers, and the reflect dressing.
"""

from __future__ import annotations

import asyncio
import json
import os
import signal as signal_mod
import socket
import subprocess
from typing import Any

from bridge_core._transport import ConnectionClosed, _BaseTransport
from bridge_core.core import _BridgeState

TUNNEL_READY_TIMEOUT = 5.0

SENTENCE = "Cross-kernel comms bridge — WS-only, asymmetric (no peer bridge needed); weak proxy."


class WSTransport(_BaseTransport):
    """websockets client connection wrapper. Frames serialize as JSON text
    (matches web_ws/_proxy.py default mode — the binary path is reserved for the
    kernel's binary_protocol; ws bridges don't currently mint binary)."""

    def __init__(self, ws: Any) -> None:
        self._ws = ws

    @classmethod
    async def connect(cls, url: str) -> "WSTransport":
        import websockets

        ws = await websockets.connect(url, max_size=2**24)
        return cls(ws)

    @property
    def closed(self) -> bool:
        return getattr(self._ws, "closed", False)

    async def send(self, frame: dict) -> None:
        try:
            await self._ws.send(json.dumps(frame, default=str))
        except Exception as e:
            raise ConnectionClosed(str(e)) from e

    async def recv(self) -> dict:
        import websockets

        try:
            raw = await self._ws.recv()
        except websockets.ConnectionClosed as e:
            raise ConnectionClosed(str(e)) from e
        if isinstance(raw, bytes):
            raw = raw.decode("utf-8")
        return json.loads(raw)

    async def close(self) -> None:
        try:
            await self._ws.close()
        except Exception:
            pass


# ─── ssh tunnel (ssh+ws) ────────────────────────────────────────


async def _open_tunnel(
    host: str, local_port: int, remote_port: int
) -> subprocess.Popen:
    """Spawn `ssh -L local:localhost:remote -N <host>`. No `-f` (fork) — we keep
    the Popen handle to SIGTERM it on shutdown. ExitOnForwardFailure surfaces a
    local-port conflict; ServerAliveInterval keeps the tunnel from silently dying."""
    cmd = [
        "ssh",
        "-N",
        "-L",
        f"{local_port}:localhost:{remote_port}",
        "-o",
        "ExitOnForwardFailure=yes",
        "-o",
        "ServerAliveInterval=15",
        "-o",
        "ServerAliveCountMax=3",
        host,
    ]
    proc = subprocess.Popen(
        cmd,
        stdin=subprocess.DEVNULL,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
        start_new_session=True,
    )
    deadline = asyncio.get_event_loop().time() + TUNNEL_READY_TIMEOUT
    while asyncio.get_event_loop().time() < deadline:
        if proc.poll() is not None:
            raise RuntimeError(f"ssh tunnel exited early (code {proc.returncode})")
        try:
            with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as s:
                s.settimeout(0.2)
                s.connect(("127.0.0.1", local_port))
                return proc
        except OSError:
            await asyncio.sleep(0.1)
    proc.terminate()
    raise TimeoutError(
        f"ssh tunnel to {host}:{remote_port} not ready in {TUNNEL_READY_TIMEOUT}s"
    )


def _kill_tunnel(proc: subprocess.Popen | None) -> None:
    if proc is None or proc.poll() is not None:
        return
    try:
        os.killpg(os.getpgid(proc.pid), signal_mod.SIGTERM)
    except OSError:
        try:
            proc.terminate()
        except OSError:
            return
    try:
        proc.wait(timeout=2.0)
    except subprocess.TimeoutExpired:
        try:
            os.killpg(os.getpgid(proc.pid), signal_mod.SIGKILL)
        except OSError:
            try:
                proc.kill()
            except OSError:
                pass


# ─── the build_transport seam ───────────────────────────────────


async def build_transport(
    kind: str, rec: dict, kernel: Any, st: _BridgeState
) -> _BaseTransport:
    """Build a `ws` or `ssh+ws` transport from the agent record. Raises on bad
    config / connect failure (the engine turns that into a boot error). For
    `ssh+ws` it opens the tunnel, stashes the pid for reflect, and registers
    `st.cleanup` so the engine tears the tunnel down on shutdown."""
    peer_id = rec.get("peer_id")
    if not peer_id:
        raise ValueError("peer_id required for ws/ssh+ws transports")

    if kind == "ws":
        port_val = rec.get("local_port") or rec.get("remote_port")
        if not port_val:
            raise ValueError("ws transport requires local_port or remote_port")
        host = rec.get("host") or "localhost"
        url = f"ws://{host}:{int(port_val)}/{peer_id}/ws"
        return await WSTransport.connect(url)

    if kind == "ssh+ws":
        host = rec.get("host")
        local_port = int(rec.get("local_port") or 0)
        remote_port_val = rec.get("remote_port")
        if not host or not local_port or not remote_port_val:
            raise ValueError("ssh+ws requires host, local_port, remote_port")
        tunnel = await _open_tunnel(host, local_port, int(remote_port_val))
        st.extra["tunnel_pid"] = tunnel.pid
        st.cleanup = lambda: _kill_tunnel(tunnel)
        try:
            return await WSTransport.connect(
                f"ws://localhost:{local_port}/{peer_id}/ws"
            )
        except Exception:
            _kill_tunnel(tunnel)
            st.cleanup = None
            st.extra.pop("tunnel_pid", None)
            raise

    raise ValueError(f"unknown transport {kind!r}")


def reflect_fields(rec: dict, st: _BridgeState) -> dict:
    """WS-flavored reflect fields."""
    return {
        "host": rec.get("host"),
        "peer_id": rec.get("peer_id"),
        "local_port": rec.get("local_port"),
        "remote_port": rec.get("remote_port"),
        "tunnel_pid": st.extra.get("tunnel_pid"),
    }
