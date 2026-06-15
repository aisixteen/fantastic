"""relay_connector transport seam — a relay-KERNEL router (../fantastic_relay).

The relay is itself a kernel: each connection is a `peer_proxy` agent addressed
by a GUID. We dial `ws://<host>/<guid>` with the group password in the
`X-Fantastic-Auth` header (checked ONCE at the WS upgrade) and the
`fantastic.relay.v1` subprotocol. The relay routes by `target`: a frame to a
peer GUID arrives on that peer's socket as `{type:"event", source:<our guid>,
payload}` — **one-way**. There is NO relay-level reply correlation for
peer→peer (a `call` to a peer returns only the relay's delivery ack).

So this transport TUNNELS the shared io_bridge engine's bridge frames
(`call`/`reply`/`watch`/`event`) inside relay `send` frames addressed to a fixed
`partner_guid`, and unwraps the partner's `event.payload` back into bridge
frames on the way in. The engine then does forward/reply correlation and
symmetric inbound serving EXACTLY as for ws_bridge — BOTH peers run a
relay_connector, so the callee role works.

Wire: PURE STREAMS via the shared io_bridge codec — the SAME encoding ws_bridge
and web_ws use, no base64. A bridge frame is wrapped in a relay envelope
`{type:"send", target:<partner>, payload:<bridge frame>}` and run through
`encode_frame`: a control frame goes as a TEXT WS frame (UTF-8 JSON); a frame
carrying a raw `read_stream` chunk goes as a BINARY WS frame
`[4B len | JSON header | raw body]` — bytes ride RAW, never base64.

Resilience: the connection is SELF-HEALING. `recv()` owns the socket lifecycle —
on a drop it re-dials after `reconnect` seconds (default 10; `0` = one-shot, the
legacy behavior) and keeps trying, transparently to the engine's read loop. The
initial dial is attempted eagerly at boot; if it fails AND reconnect is on, boot
still succeeds and the leg connects in the background (auto-connect). `reflect`
surfaces the LIVE socket state via `connected`.
"""

from __future__ import annotations

import asyncio
import json
from typing import Any, Awaitable, Callable

from io_bridge import (
    ConnectionClosed,
    _BaseTransport,
    _BridgeState,
    decode_frame,
    encode_frame,
)

SUBPROTOCOL = "fantastic.relay.v1"
DEFAULT_HEARTBEAT = 30.0
DEFAULT_RECONNECT = 10.0
MAX_FRAME = 2**24

SENTENCE = (
    "Cross-kernel comms through a relay-kernel router — dial-out WS, "
    "group-password auth (X-Fantastic-Auth), GUID-routed; symmetric RPC "
    "tunneled over the relay; auto-reconnecting."
)

# An async dialer: `() -> ws`. Returns a connected websockets client (or raises).
Dialer = Callable[[], Awaitable[Any]]


def _default_dialer(relay_url: str, guid: str, token: str) -> Dialer:
    url = f"{relay_url.rstrip('/')}/{guid}"

    async def dial() -> Any:
        import websockets

        return await websockets.connect(
            url,
            subprotocols=[SUBPROTOCOL],
            additional_headers={"X-Fantastic-Auth": token},
            max_size=MAX_FRAME,
        )

    return dial


class RelayTransport(_BaseTransport):
    """Tunnels io_bridge frames over a (self-healing) relay-kernel connection to
    one `partner`. `send`/`recv` wrap/unwrap the relay envelope; `recv` re-dials
    on a drop with the `reconnect` backoff. Only `{type:"event", source:<partner>}`
    is a tunnel delivery — directory events / relay acks / other peers are skipped."""

    def __init__(
        self,
        *,
        dialer: Dialer,
        partner_guid: str,
        reconnect: float = DEFAULT_RECONNECT,
        heartbeat: float = DEFAULT_HEARTBEAT,
        ws: Any = None,
    ) -> None:
        self._dialer = dialer
        self._partner = partner_guid
        self._reconnect = reconnect
        self._heartbeat = heartbeat
        self._ws = ws
        self._closed = False
        self._hb_task: asyncio.Task | None = None
        # Relay-LEVEL request correlation (the directory `call`/`watch target:relay`
        # — distinct from the engine's partner bridge-frame correlation).
        self._relay_pending: dict[str, asyncio.Future] = {}
        self._relay_next_id = 0
        if ws is not None:
            self._start_heartbeat()

    @classmethod
    async def connect(
        cls,
        relay_url: str,
        guid: str,
        token: str,
        partner_guid: str,
        heartbeat: float = DEFAULT_HEARTBEAT,
        reconnect: float = DEFAULT_RECONNECT,
        dialer: Dialer | None = None,
    ) -> "RelayTransport":
        dialer = dialer or _default_dialer(relay_url, guid, token)
        t = cls(
            dialer=dialer,
            partner_guid=partner_guid,
            reconnect=reconnect,
            heartbeat=heartbeat,
        )
        # Eager first dial. On failure: one-shot (reconnect<=0) raises so boot
        # fails loudly; otherwise swallow it — recv() auto-connects in the
        # background (the leg boots, then connects + maintains).
        try:
            t._ws = await dialer()
            t._start_heartbeat()
        except Exception:
            if reconnect <= 0:
                raise
            t._ws = None
        return t

    @property
    def is_live(self) -> bool:
        """A usable socket exists right now (not closed, not mid-reconnect)."""
        if self._closed or self._ws is None:
            return False
        state = getattr(self._ws, "state", None)
        return state is None or getattr(state, "name", "OPEN") == "OPEN"

    @property
    def closed(self) -> bool:
        # EXPLICIT close only — a transient drop is healed by recv() (reconnect),
        # so the leg stays alive across blips (the engine doesn't tear it down).
        return self._closed

    def _start_heartbeat(self) -> None:
        if self._hb_task is not None:
            self._hb_task.cancel()
        self._hb_task = (
            asyncio.create_task(self._heartbeat_loop()) if self._heartbeat > 0 else None
        )

    async def _reconnect_ws(self) -> Any | None:
        """(Re)dial with the backoff. Returns a live ws, or None if explicitly
        closed. Raises ConnectionClosed only in one-shot mode (reconnect<=0)."""
        while not self._closed:
            try:
                self._ws = await self._dialer()
                self._start_heartbeat()
                return self._ws
            except Exception as e:
                self._ws = None
                if self._reconnect <= 0:
                    raise ConnectionClosed(f"relay connect failed: {e}") from e
                await asyncio.sleep(self._reconnect)
        return None

    async def send(self, frame: dict) -> None:
        ws = self._ws
        if ws is None:
            raise ConnectionClosed("relay_connector: not connected")
        envelope = {"type": "send", "target": self._partner, "payload": frame}
        wire, is_binary = encode_frame(envelope)
        try:
            await ws.send(wire if is_binary else wire.decode("utf-8"))
        except Exception as e:  # the socket is gone; recv() will re-dial.
            self._ws = None
            raise ConnectionClosed(str(e)) from e

    # ── directory surface (the relay's own `relay` agent) ──────────

    async def _relay_request(self, frame: dict, timeout: float) -> dict:
        """Send a relay-LEVEL frame to the directory (`target:"relay"`, with a
        minted relay id) and await the correlated `{type:"reply", id, data}`.
        Bypasses the partner tunnel — directory frames are not `send`-wrapped."""
        ws = self._ws
        if ws is None:
            return {
                "error": "relay_connector: not connected",
                "reason": "not_connected",
            }
        self._relay_next_id += 1
        rid = f"dir_{self._relay_next_id}"
        out = {**frame, "id": rid, "target": "relay"}
        fut: asyncio.Future = asyncio.get_event_loop().create_future()
        self._relay_pending[rid] = fut
        try:
            wire, is_binary = encode_frame(out)
            await ws.send(wire if is_binary else wire.decode("utf-8"))
        except Exception as e:
            self._relay_pending.pop(rid, None)
            self._ws = None
            return {
                "error": f"relay_connector: directory send failed: {e}",
                "reason": "transport_error",
            }
        try:
            return await asyncio.wait_for(fut, timeout)
        except asyncio.TimeoutError:
            self._relay_pending.pop(rid, None)
            return {
                "error": f"relay_connector: directory timeout after {timeout}s",
                "reason": "timeout",
            }

    async def list_peers(self, timeout: float = 30.0) -> dict:
        """One-shot directory snapshot → `{peers:[{guid,status,last_seen,since}]}`."""
        return await self._relay_request(
            {"type": "call", "payload": {"type": "list_peers"}}, timeout
        )

    async def watch_directory(self, timeout: float = 10.0) -> dict:
        """Subscribe to the relay directory; inbound `peer_joined|left|evicted|
        peer_status` events re-emit on this connector's inbox (via recv→engine)."""
        return await self._relay_request({"type": "watch"}, timeout)

    async def unwatch_directory(self) -> dict:
        """Stop the directory subscription (the relay sends no reply for unwatch)."""
        ws = self._ws
        if ws is None:
            return {"ok": True}
        try:
            wire, is_binary = encode_frame({"type": "unwatch", "target": "relay"})
            await ws.send(wire if is_binary else wire.decode("utf-8"))
        except Exception:
            self._ws = None
        return {"ok": True, "unwatched": "relay"}

    async def recv(self) -> dict:
        while not self._closed:
            ws = self._ws
            if ws is None:
                if await self._reconnect_ws() is None:
                    break
                continue
            try:
                raw = await ws.recv()
            except Exception:  # drop (incl. websockets.ConnectionClosed)
                self._ws = None
                if self._reconnect <= 0:
                    raise ConnectionClosed("relay_connector: connection dropped")
                await asyncio.sleep(self._reconnect)
                continue
            # str ⇒ text JSON frame; bytes ⇒ binary [len|header|body] (raw body
            # restored at its path). The codec is the same one ws_bridge/web_ws use.
            try:
                msg = decode_frame(raw)
            except Exception:
                continue
            mtype = msg.get("type")
            # Relay-LEVEL reply (a directory `list_peers`/`watch` ack) — resolve
            # the pending request; never surfaces to the engine's bridge path.
            if mtype == "reply":
                fut = self._relay_pending.pop(msg.get("id"), None)
                if fut is not None and not fut.done():
                    fut.set_result(msg.get("data") or {})
                continue
            if mtype == "event":
                source = msg.get("source")
                payload = msg.get("payload")
                if not isinstance(payload, dict):
                    continue
                # A DIRECTORY event (source="relay") — surface it as a bridge
                # `event` so the engine re-emits it on this connector's inbox
                # (kernel.watch(<connector>) sees peer_joined|left|evicted|peer_status).
                if source == "relay":
                    return {"type": "event", "payload": payload}
                # A tunnel delivery FROM our partner — the inner bridge frame.
                if source == self._partner:
                    return payload
            # any other peer / type → skip
        raise ConnectionClosed("relay_connector: closed")

    async def close(self) -> None:
        self._closed = True
        if self._hb_task is not None:
            self._hb_task.cancel()
        for rid, fut in list(self._relay_pending.items()):
            if not fut.done():
                fut.set_result(
                    {"error": "relay_connector: closed", "reason": "transport_closed"}
                )
        self._relay_pending.clear()
        if self._ws is not None:
            try:
                await self._ws.close()
            except Exception:
                pass
        self._ws = None

    async def _heartbeat_loop(self) -> None:
        """Keep the relay peer `green`. Any inbound .text frame refreshes the
        relay's `last_seen` (CONTRACT §4); the relay's `keepalive` verb is a
        no-reply refresh — exactly that, with no wire noise."""
        keepalive = json.dumps({"type": "keepalive"})
        try:
            while not self._closed:
                await asyncio.sleep(self._heartbeat)
                ws = self._ws
                if ws is None:
                    return  # disconnected — recv() restarts the heartbeat on re-dial.
                try:
                    await ws.send(keepalive)
                except Exception:
                    return
        except asyncio.CancelledError:
            pass


# ─── the build_transport seam ───────────────────────────────────


async def build_transport(
    kind: str, rec: dict, kernel: Any, st: _BridgeState
) -> _BaseTransport:
    """Build a relay transport from the agent record. Required: `relay_url`
    (ws://host:port), `guid` (our id — the WS path), `partner_guid` (the peer to
    reach). Optional: `relay_token` (X-Fantastic-Auth; default ""), `heartbeat`
    (s, default 30; 0 off), `reconnect` (s before each re-dial, default 10; 0 =
    one-shot — boot fails if the relay is down)."""
    relay_url = rec.get("relay_url")
    guid = rec.get("guid")
    partner_guid = rec.get("partner_guid")
    if not relay_url or not guid or not partner_guid:
        raise ValueError("relay_connector requires relay_url, guid, partner_guid")
    token = rec.get("relay_token") or ""
    heartbeat = float(rec.get("heartbeat", DEFAULT_HEARTBEAT))
    reconnect = float(rec.get("reconnect", DEFAULT_RECONNECT))
    return await RelayTransport.connect(
        str(relay_url),
        str(guid),
        str(token),
        str(partner_guid),
        heartbeat=heartbeat,
        reconnect=reconnect,
    )


def reflect_fields(rec: dict, st: _BridgeState) -> dict:
    """Relay-flavored reflect fields. Overrides the engine's `connected` (which
    only checks the transport EXISTS) with the LIVE socket state — a leg
    mid-reconnect reads `connected:false`."""
    transport = st.transport
    live = (
        bool(getattr(transport, "is_live", False)) if transport is not None else False
    )
    return {
        "connected": live,
        "relay_url": rec.get("relay_url"),
        "guid": rec.get("guid"),
        "partner_guid": rec.get("partner_guid"),
        "reconnect": float(rec.get("reconnect", DEFAULT_RECONNECT)),
    }
