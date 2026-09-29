"""ws_bridge transport seam — WS to a remote `web_ws`.

The bridge is an **asymmetric pure client**: it dials a remote kernel's
`web_ws` surface at `ws://{host}:{port}/{peer_id}/ws` and ships raw bridge
frames (`call`/`reply`/`watch`/`event`). The dispatch engine + verbs live in
`io_bridge`; this module supplies only the `ws` transport builder and the
reflect dressing. It spawns nothing: a tunnel to a remote host is an
`exec_bridge` job (`ssh -N -L …` under an operator's allowlist) or devops —
the bridge then dials its local end like any other `ws://host:port`.
"""

from __future__ import annotations

import logging
from typing import Any

from io_bridge import ConnectionClosed, _BaseTransport, decode_frame, encode_frame
from io_bridge import _BridgeState

logger = logging.getLogger(__name__)

SENTENCE = "Cross-kernel comms bridge — WS-only, asymmetric (no peer bridge needed); weak proxy."


class WSTransport(_BaseTransport):
    """websockets client connection wrapper. Frames serialize via the shared
    `io_bridge` codec: a frame carrying raw `bytes` (a `read_stream` chunk) goes as a
    BINARY WS frame `[len|header|body]` — no base64; a plain frame goes as a TEXT WS
    frame (UTF-8 JSON). web_ws on the far side decodes both via the same codec."""

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
        wire, is_binary = encode_frame(frame)
        try:
            # websockets sends a bytes arg as a binary frame, a str as a text frame —
            # so the text/binary distinction the codec needs survives on the wire.
            await self._ws.send(wire if is_binary else wire.decode("utf-8"))
        except Exception as e:
            raise ConnectionClosed(str(e)) from e

    async def recv(self) -> dict:
        """Next DECODABLE frame. A malformed frame is skipped, not fatal: the peer
        is remote, so a short binary header or bad JSON is an input, not a bug —
        raising here killed the engine read loop while `st.transport` stayed open,
        leaving a bridge that reflects `connected:true` and answers every forward
        with a 30s timeout that never names the dead reader."""
        import websockets

        while True:
            try:
                raw = await self._ws.recv()
            except websockets.ConnectionClosed as e:
                raise ConnectionClosed(str(e)) from e
            # str ⇒ text (JSON) frame; bytes ⇒ binary [len|header|body] frame.
            try:
                frame = decode_frame(raw)
            except Exception as e:
                logger.warning("ws_bridge: skipping malformed frame: %s", e)
                continue
            if not isinstance(frame, dict):
                logger.warning("ws_bridge: skipping non-object frame")
                continue
            return frame

    async def close(self) -> None:
        try:
            await self._ws.close()
        except Exception:
            pass


# ─── the build_transport seam ───────────────────────────────────


async def build_transport(
    kind: str, rec: dict, kernel: Any, st: _BridgeState
) -> _BaseTransport:
    """Build the `ws` transport from the agent record. Raises on bad config /
    connect failure (the engine turns that into a boot error). `host` is
    explicit — nothing is guessed."""
    peer_id = rec.get("peer_id")
    if not peer_id:
        raise ValueError("peer_id required for the ws transport")

    if kind == "ws":
        port = rec.get("port")
        if not port:
            raise ValueError("ws transport requires port")
        host = rec.get("host")
        if not host:
            raise ValueError("ws transport requires host (nothing is guessed)")
        url = f"ws://{host}:{int(port)}/{peer_id}/ws"
        return await WSTransport.connect(url)

    raise ValueError(f"unknown transport {kind!r}")


def reflect_fields(rec: dict, st: _BridgeState) -> dict:
    """WS-flavored reflect fields."""
    return {
        "host": rec.get("host"),
        "peer_id": rec.get("peer_id"),
        "port": rec.get("port"),
    }
