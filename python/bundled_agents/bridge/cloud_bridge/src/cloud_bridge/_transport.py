"""cloud_bridge transport — dial-OUT relay leg with end-to-end TLS 1.3 mTLS.

`CloudBridgeTransport` is a `bridge_core._BaseTransport`: it dials OUT (WSS) to the
relay, authenticates the WS with a subprotocol token, is paired by
`(tenant_id, rendezvous)`, then runs a **mutually-authenticated TLS 1.3 handshake
over the relay's opaque byte pipe** (pyOpenSSL over a memory BIO, no socket — see
`_tls.py`). After the handshake it tunnels the SAME kernel-bridge `call/reply/event`
frames as **TLS application data**, length-delimited, shipped as opaque binary WS
frames. The relay forwards ciphertext + sees nothing; a forged route fails the TLS
handshake (the impostor can't prove control of an approved device's pinned key).
"""

from __future__ import annotations

import asyncio
import json
import struct
from typing import Any

from bridge_core._transport import ConnectionClosed, _BaseTransport

SUBPROTOCOL = "fantastic.relay.v1"
KEEPALIVE_TYPE = "keepalive"
HANDSHAKE_TIMEOUT = 15.0  # < the relay's 30s pair timeout
MAX_FRAME = 16 * 1024 * 1024  # reassembly cap (matches the relay's 16 MiB)
_LEN = struct.Struct(">I")  # big-endian length prefix per frame
_HDR = _LEN.size  # 4 — bytes of length prefix
_READ = 65536


def _drain(conn) -> bytes:
    """Pull all available outbound ciphertext out of the connection's BIO."""
    from OpenSSL import SSL

    chunks = []
    while True:
        try:
            chunks.append(conn.bio_read(_READ))
        except SSL.WantReadError:
            break
    return b"".join(chunks)


async def _drive(ws: Any, conn, fn):
    """Run a TLS op (`do_handshake` / `send`) to completion over the WS pipe:
    flush outbound records the op produces, feed inbound records it needs."""
    from OpenSSL import SSL

    while True:
        try:
            ret = fn()
        except SSL.WantReadError:
            data = _drain(conn)
            if data:
                await ws.send(data)
            raw = await ws.recv()
            conn.bio_write(raw if isinstance(raw, bytes) else raw.encode("utf-8"))
            continue
        except SSL.WantWriteError:
            data = _drain(conn)
            if data:
                await ws.send(data)
            continue
        else:
            data = _drain(conn)
            if data:
                await ws.send(data)
            return ret


class CloudBridgeTransport(_BaseTransport):
    def __init__(self, ws, conn, peer_identity: dict, heartbeat: float) -> None:
        self._ws = ws
        self._conn = conn  # OpenSSL.SSL.Connection in memory-BIO mode
        self.peer_identity = peer_identity  # {"pubkey": bytes}
        self._send_lock = asyncio.Lock()
        self._rbuf = bytearray()
        self._closed = False
        self._hb_interval = heartbeat
        self._hb_task: asyncio.Task | None = (
            asyncio.create_task(self._heartbeat())
            if heartbeat and heartbeat > 0
            else None
        )

    @classmethod
    async def connect(
        cls,
        relay_url: str,
        token: str,
        *,
        server: bool,
        cert_pem: bytes,
        key_pem: bytes,
        approved_certs_pem: list[bytes],
        expected_partner_pubkey: bytes | None = None,
        heartbeat: float = 30.0,
    ) -> "CloudBridgeTransport":
        import websockets
        from OpenSSL import SSL

        from cloud_bridge._tls import make_context

        ws = await websockets.connect(
            relay_url, subprotocols=[SUBPROTOCOL, token], max_size=MAX_FRAME
        )
        try:
            ctx = make_context(
                server=server,
                cert_pem=cert_pem,
                key_pem=key_pem,
                approved_certs_pem=approved_certs_pem,
            )
            conn = SSL.Connection(ctx, None)  # None socket → memory BIO
            conn.set_accept_state() if server else conn.set_connect_state()
            # Bounded handshake — a paired-but-silent peer must not hang boot forever.
            # The verify callback already pinned the peer's pubkey; a forged/un-
            # approved peer fails here.
            await asyncio.wait_for(
                _drive(ws, conn, conn.do_handshake), HANDSHAKE_TIMEOUT
            )
            peer = conn.get_peer_certificate()
            peer_pub = (
                peer.to_cryptography().public_key().public_bytes_raw()
                if peer is not None
                else None
            )
            if expected_partner_pubkey and peer_pub != expected_partner_pubkey:
                raise ConnectionClosed("cloud_bridge: peer cert != expected partner")
            return cls(ws, conn, {"pubkey": peer_pub}, heartbeat)
        except Exception:
            try:
                await ws.close()
            except Exception:
                pass
            raise

    @property
    def closed(self) -> bool:
        return self._closed or getattr(self._ws, "closed", False)

    async def _heartbeat(self) -> None:
        try:
            while not self._closed:
                await asyncio.sleep(self._hb_interval)
                if self._closed:
                    return
                try:
                    await self.send({"type": KEEPALIVE_TYPE})
                except ConnectionClosed:
                    return
        except asyncio.CancelledError:
            pass

    async def send(self, frame: dict) -> None:
        data = json.dumps(frame, default=str).encode("utf-8")
        payload = _LEN.pack(len(data)) + data  # length-delimited inside the TLS stream
        async with self._send_lock:
            try:
                await _drive(self._ws, self._conn, lambda: self._conn.sendall(payload))
            except Exception as e:
                raise ConnectionClosed(str(e)) from e

    async def _fill(self) -> None:
        """Pull one batch of decrypted bytes into `_rbuf`, feeding inbound WS
        records into the TLS engine as needed."""
        import websockets
        from OpenSSL import SSL

        while True:
            try:
                chunk = self._conn.recv(_READ)
            except SSL.WantReadError:
                try:
                    raw = await self._ws.recv()
                except websockets.ConnectionClosed as e:
                    raise ConnectionClosed(str(e)) from e
                self._conn.bio_write(
                    raw if isinstance(raw, bytes) else raw.encode("utf-8")
                )
                continue
            except SSL.ZeroReturnError as e:
                raise ConnectionClosed("cloud_bridge: peer closed TLS") from e
            except SSL.Error as e:
                raise ConnectionClosed(f"cloud_bridge TLS error: {e}") from e
            if not chunk:
                raise ConnectionClosed("cloud_bridge: peer closed TLS")
            self._rbuf += chunk
            if len(self._rbuf) > MAX_FRAME + _HDR:
                raise ConnectionClosed("cloud_bridge: frame exceeds cap")
            return

    async def _recv_frame(self) -> dict:
        while True:
            if len(self._rbuf) >= _HDR:
                (n,) = _LEN.unpack_from(self._rbuf, 0)
                if n > MAX_FRAME:
                    raise ConnectionClosed("cloud_bridge: frame exceeds cap")
                if len(self._rbuf) >= _HDR + n:
                    data = bytes(self._rbuf[_HDR : _HDR + n])
                    del self._rbuf[: _HDR + n]
                    return json.loads(data)
            await self._fill()

    async def recv(self) -> dict:
        while True:
            frame = await self._recv_frame()
            if frame.get("type") == KEEPALIVE_TYPE:
                continue  # heartbeat — never surface to the bridge read loop
            return frame

    async def close(self) -> None:
        self._closed = True
        if self._hb_task is not None and not self._hb_task.done():
            self._hb_task.cancel()
        try:
            await self._ws.close()
        except Exception:
            pass
