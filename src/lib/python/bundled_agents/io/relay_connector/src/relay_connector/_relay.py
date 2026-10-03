"""relay_connector transport seam — a relay-KERNEL router (../fantastic_relay).

The relay is itself a kernel: each connection is a `peer_proxy` agent addressed
by a GUID. We dial `ws://<host>/<guid>` with the group password in the
`X-Fantastic-Auth` header (checked ONCE at the WS upgrade) and the
`fantastic.relay.v1` subprotocol. The password is a SECRET and lives only in a
credential_store (audit IO-7/F4, 2026-10-03): the record names it —
`credentials: {store, ref}` — and before EVERY dial the connector reads the ref's
`password` field as itself, the answer marked `secret: ['result']`, pinned to the
relay URL's host, exactly as an AI backend reads its key (`ai_core/credentials`).
Nothing is cached: a rotated password is used by the next dial. The relay routes by `target`: a frame to a
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
on a drop it re-dials after `reconnect` seconds (default 10; `0` = one-shot) and
keeps trying, transparently to the engine's read loop. The
initial dial is attempted eagerly at boot; if it fails AND reconnect is on, boot
still succeeds and the leg connects in the background (auto-connect). `reflect`
surfaces the LIVE socket state via `connected`.
"""

from __future__ import annotations

import asyncio
import json
import uuid
from typing import Any, Awaitable, Callable

from io_bridge import (
    ConnectionClosed,
    _BaseTransport,
    _BridgeState,
    decode_frame,
    encode_frame,
)
from kernel import sender_context
from kernel._refusal import UNAUTHORIZED, refusal

SUBPROTOCOL = "fantastic.relay.v1"

# THE RELAY SEAL (owner, 2026-10-02). relayd dispatches every tunneled frame into
# its own kernel and records it UNMASKED, so a secret that crossed a relay leg
# would sit in the relay's ledger in full. Until relayd masks what it records, a
# relay leg refuses any call carrying secret marks, in both directions: outbound
# in `tools._forward`, inbound in `RelayTransport.recv`. ws_bridge (direct WS) is
# sealed only to a non-loopback host (its own seal, `ws_bridge._ws`). One text for
# both.
SECRET_SEALED = (
    "relay_connector: a call carrying secret marks may not cross a relay leg — "
    "the relay records every frame it routes, unmasked"
)
SECRET_SEALED_HINT = "reach that kernel over a direct ws_bridge leg to send a secret"


def secret_sealed() -> dict:
    """The relay seal's refusal — outbound to the local caller, inbound as the
    reply to the far caller."""
    return refusal(UNAUTHORIZED, SECRET_SEALED, hint=SECRET_SEALED_HINT)


DEFAULT_HEARTBEAT = 30.0
DEFAULT_RECONNECT = 10.0
MAX_FRAME = 2**24

#: The relay's own directory agent: a directory request is addressed `target` to
#: it, and a directory event reaches the engine as an `event` frame from `src` it —
#: so `watch_directory` subscribes the leg to it like any watched agent.
DIRECTORY_SRC = "relay"

SENTENCE = (
    "Cross-kernel comms through a relay-kernel router — dial-out WS, "
    "group-password auth (X-Fantastic-Auth), GUID-routed; symmetric RPC "
    "tunneled over the relay; auto-reconnecting."
)

# An async dialer: `() -> ws`. Returns a connected websockets client (or raises).
Dialer = Callable[[], Awaitable[Any]]
# The relay password, read at dial time: `() -> str` (raises naming store + ref).
PasswordReader = Callable[[], Awaitable[str]]

#: The credential field that holds the relay group password.
FIELD = "password"


class CredentialError(RuntimeError):
    """The relay password could not be read — the dial fails, naming the store
    and the ref."""


def credentials_shape_error(v) -> str | None:
    """Why a `credentials` value is malformed, or None: a map of exactly a
    non-empty `store` id and a non-empty `ref` (ai_core's shape, its texts)."""
    if not isinstance(v, dict) or set(v) != {"store", "ref"}:
        return "credentials must be {store: <credential_store id>, ref: <ref>}"
    for k in ("store", "ref"):
        if not isinstance(v[k], str) or not v[k].strip():
            return f"credentials.{k} must be a non-empty string"
    return None


def password_reader(self_id: str, kernel: Any, relay_url: str) -> PasswordReader:
    """The async `read() -> password` a dial awaits — every dial, nothing cached.
    Asks the store named by the record's `credentials` AS THIS CONNECTOR for the
    ref's `password`, the answer marked secret (it lands in no row, ledger or
    stream) and pinned to the relay URL's host (the store refuses a ref bound
    elsewhere). Raises `CredentialError` on any refusal or a missing field."""

    async def read() -> str:
        cred = (kernel.get(self_id) or {}).get("credentials")
        bad = credentials_shape_error(cred)
        if bad:
            raise CredentialError(f"relay_connector: {bad}")
        store, ref = cred["store"], cred["ref"]
        with sender_context(self_id):
            out = (
                await kernel.send(
                    [
                        {
                            "target": store,
                            "payload": {"type": "get", "ref": ref, "host": relay_url},
                            "secret": ["result"],
                        }
                    ]
                )
            )[0]
        res = out.get("result")
        if (
            out.get("status") != "completed"
            or not isinstance(res, dict)
            or res.get("error")
        ):
            err = res.get("error") if isinstance(res, dict) else out.get("status")
            raise CredentialError(f"relay_connector: credentials {store}/{ref}: {err}")
        pw = (res.get("fields") or {}).get(FIELD)
        if not isinstance(pw, str) or not pw:
            raise CredentialError(
                f"relay_connector: credential {ref!r} in {store!r} has no {FIELD!r} "
                "field"
            )
        return pw

    return read


async def _open_ws(url: str, headers: dict) -> Any:
    """THE network dial — the one place a relay socket opens."""
    import websockets

    return await websockets.connect(
        url,
        subprotocols=[SUBPROTOCOL],
        additional_headers=headers,
        max_size=MAX_FRAME,
    )


def _default_dialer(relay_url: str, guid: str, password: PasswordReader) -> Dialer:
    url = f"{relay_url.rstrip('/')}/{guid}"

    async def dial() -> Any:
        # Read at USE time, every (re)dial: a rotated password reaches the relay
        # on the next connect, and no copy outlives this call.
        return await _open_ws(url, {"X-Fantastic-Auth": await password()})

    return dial


class RelayTransport(_BaseTransport):
    """Tunnels io_bridge frames over a (self-healing) relay-kernel connection to
    one `partner`. `send`/`recv` wrap/unwrap the relay envelope; `recv` re-dials
    on a drop with the `reconnect` backoff. Only `{type:"event", source:<partner>}`
    is a tunnel delivery; a directory event surfaces as an `event` frame from
    `src:"relay"`; relay acks resolve directory requests; other peers are skipped."""

    def __init__(
        self,
        *,
        dialer: Dialer,
        partner_guid: str,
        reconnect: float = DEFAULT_RECONNECT,
        heartbeat: float = DEFAULT_HEARTBEAT,
        identity: dict | None = None,
        ws: Any = None,
        spawn: Any = None,
    ) -> None:
        self._dialer = dialer
        # HOW THIS TRANSPORT FORKS. Production passes the kernel's
        # `spawn_task`-backed callable, so the heartbeat is a `ps` row under its
        # connector and `cancel` reaches it; a bare task left a loop that pings
        # the relay forever with nobody able to see or stop it. A test that
        # builds a transport directly has no kernel and gets a plain task.
        self._spawn = spawn or (lambda _name, coro: asyncio.create_task(coro))
        self._partner = partner_guid
        self._reconnect = reconnect
        self._heartbeat = heartbeat
        # Directory attributes we advertise to the relay (opaque blob — the relay
        # stores + reflects it, never interprets it). `{}` ⇒ a plain peer, no
        # announce sent (byte-identical to an un-typed leg).
        self._identity: dict = identity or {}
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
        password: PasswordReader,
        partner_guid: str,
        heartbeat: float = DEFAULT_HEARTBEAT,
        reconnect: float = DEFAULT_RECONNECT,
        identity: dict | None = None,
        dialer: Dialer | None = None,
        spawn: Any = None,
    ) -> "RelayTransport":
        dialer = dialer or _default_dialer(relay_url, guid, password)
        t = cls(
            dialer=dialer,
            partner_guid=partner_guid,
            reconnect=reconnect,
            heartbeat=heartbeat,
            identity=identity,
            spawn=spawn,
        )
        # Eager first dial. On failure: one-shot (reconnect<=0) raises so boot
        # fails loudly; otherwise swallow it — recv() auto-connects in the
        # background (the leg boots, then connects + maintains).
        try:
            t._ws = await dialer()
            await t._on_socket_open()
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
            self._spawn("relay_heartbeat", self._heartbeat_loop())
            if self._heartbeat > 0
            else None
        )

    async def _on_socket_open(self) -> None:
        """Run after EVERY successful (re)dial: start the heartbeat and re-advertise
        our directory identity (the relay drops our state on disconnect, so the
        announce must ride each new socket)."""
        self._start_heartbeat()
        await self._announce()

    async def _announce(self) -> None:
        """Advertise our directory attributes to the relay (`target:"relay"`,
        fire-and-forget — the relay reflects them into `list_peers` and emits a
        `peer_updated` event). No-op for a plain peer (`identity == {}`)."""
        if not self._identity:
            return
        ws = self._ws
        if ws is None:
            return
        try:
            wire, is_binary = encode_frame(
                {"type": "announce", "attrs": self._identity, "target": "relay"}
            )
            await ws.send(wire if is_binary else wire.decode("utf-8"))
        except Exception:
            self._ws = None  # the socket is gone; recv() will re-dial + re-announce.

    async def set_identity(self, attrs: dict) -> dict:
        """Replace the advertised directory attributes and re-announce now (a manager
        publishing/retracting a kernel's control surface). The caller persists the
        new attrs into the record so they re-announce on the next boot."""
        self._identity = attrs or {}
        await self._announce()
        return {"ok": True, "attrs": self._identity}

    async def _reconnect_ws(self) -> Any | None:
        """(Re)dial with the backoff. Returns a live ws, or None if explicitly
        closed. Raises ConnectionClosed only in one-shot mode (reconnect<=0)."""
        while not self._closed:
            try:
                self._ws = await self._dialer()
                await self._on_socket_open()
            except Exception as e:
                self._ws = None
                if self._reconnect <= 0:
                    raise ConnectionClosed(f"relay connect failed: {e}") from e
                await asyncio.sleep(self._reconnect)
                continue
            if self._ws is None:
                # The dial succeeded but the socket died DURING the open handshake —
                # `_announce`/`_on_socket_open` nulls `_ws` when its first send fails
                # (a relay restarting, an auth window closing between upgrade and
                # first frame). Returning None here read as "explicitly closed" and
                # ended the self-healing loop for good, on a leg whose record says
                # `reconnect:10`. Keep healing: only `_closed` produces None.
                if self._reconnect <= 0:
                    raise ConnectionClosed(
                        "relay connect failed: socket closed on open"
                    )
                await asyncio.sleep(self._reconnect)
                continue
            return self._ws
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
            # A non-object frame, like an undecodable one, is skipped: raising
            # here ended the engine read loop with the leg still `connected`.
            if not isinstance(msg, dict):
                continue
            mtype = msg.get("type")
            # Relay-LEVEL reply (a directory `list_peers`/`watch` ack) — resolve
            # the pending request; never surfaces to the engine's bridge path.
            # Only a STRING id names a request of ours.
            if mtype == "reply":
                rid = msg.get("id")
                if not isinstance(rid, str):
                    continue
                fut = self._relay_pending.pop(rid, None)
                if fut is not None and not fut.done():
                    fut.set_result(msg.get("data") or {})
                continue
            if mtype == "event":
                source = msg.get("source")
                payload = msg.get("payload")
                if not isinstance(payload, dict):
                    continue
                # A DIRECTORY event (source="relay") — surface it as a bridge
                # `event` from the directory, which the engine re-emits on this
                # connector's inbox while `watch_directory` holds it in the leg's
                # `watching` (kernel.watch(<connector>) sees
                # peer_joined|left|evicted|peer_status, `src:"relay"`).
                if source == DIRECTORY_SRC:
                    return {"type": "event", "src": DIRECTORY_SRC, "payload": payload}
                # A tunnel delivery FROM our partner — the inner bridge frame.
                if source == self._partner:
                    if payload.get("type") == "call" and "secret" in payload:
                        # THE RELAY SEAL, inbound: the frame already crossed the
                        # relay, and nothing here dispatches it — the far caller is
                        # answered with the seal's refusal.
                        # TODO(relay seal): lift once relayd masks what it records.
                        # The fix is in relayd (../fantastic_relay): when it
                        # dispatches a tunneled frame into its own kernel, re-root
                        # the inner frame's `secret` onto its delivery item
                        # (`payload.<f>` -> `payload.payload.payload.<f>`), so its
                        # rows, ledger and state stream show `<secret>`; and decide
                        # how REPLY frames carry result marks — a `reply` names no
                        # marks today, so the relay would record a secret result
                        # in full.
                        try:
                            await self.send(
                                {
                                    "type": "reply",
                                    "id": payload.get("id"),
                                    "data": secret_sealed(),
                                }
                            )
                        except ConnectionClosed:
                            pass  # the socket dropped; the loop re-dials
                        continue
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


# ─── directory identity (the advertised attrs blob) ─────────────

ROLES = ("manager", "kernel")


def _identity_from_record(rec: dict) -> dict:
    """Assemble the directory-attributes blob we advertise from the record's
    well-known keys. Defaults are OMITTED (role=kernel / owner_guid=null /
    exposes=[]) so a plain peer advertises nothing — the relay's own defaults
    stand and the wire stays byte-identical to an un-typed leg. The blob is opaque
    to the relay; adding a new well-known key here needs no relay change."""
    attrs: dict = {}
    role = rec.get("role")
    if role and role != "kernel":
        attrs["role"] = role
    owner_guid = rec.get("owner_guid")
    if owner_guid is not None:
        attrs["owner_guid"] = owner_guid
    exposes = rec.get("exposes")
    if exposes:
        attrs["exposes"] = exposes
    return attrs


# ─── the build_transport seam ───────────────────────────────────


async def build_transport(
    kind: str, rec: dict, kernel: Any, st: _BridgeState
) -> _BaseTransport:
    """Build a relay transport from the agent record. Required: `relay_url`
    (ws://host:port), `partner_guid` (the peer to reach), `credentials` ({store,
    ref} — the relay password's credential_store ref, field `password`, bound to
    the relay's host; read before every dial). `guid` (our id — the WS path) is
    auto-minted on first boot if absent and PERSISTED into the record, so every
    later hydration re-dials the SAME path; an explicit `guid` always wins.
    Optional: `heartbeat` (s, default 30; 0 off), `reconnect` (s before each
    re-dial, default 10; 0 = one-shot — boot fails if the relay is down)."""
    relay_url = rec.get("relay_url")
    partner_guid = rec.get("partner_guid")
    if not relay_url or not partner_guid:
        raise ValueError("relay_connector requires relay_url, partner_guid")
    cred = rec.get("credentials")
    if cred is None:
        raise ValueError(
            f"relay_connector: no `credentials` on {rec['id']} — name the relay "
            f"password's credential store and ref (update_agent {rec['id']} "
            "credentials={store: <id>, ref: <ref>}; the ref's field `password`, "
            "bound to the relay's host)"
        )
    if bad := credentials_shape_error(cred):
        raise ValueError(f"relay_connector: {bad}")
    guid = rec.get("guid")
    if not guid:
        # Mint our own relay identity ONCE and persist it into the record. The seal:
        # auto-gen only FILLS an absent field (explicit guid wins), and once written
        # back it is never regenerated — the next hydration reads it verbatim, so the
        # WS path is stable across reboots and the partner keeps finding us.
        guid = uuid.uuid4().hex
        kernel.update(rec["id"], guid=guid)
    role = rec.get("role")
    if role is not None and role not in ROLES:
        raise ValueError(f"relay_connector role must be one of {ROLES}, got {role!r}")
    heartbeat = float(rec.get("heartbeat", DEFAULT_HEARTBEAT))
    reconnect = float(rec.get("reconnect", DEFAULT_RECONNECT))
    return await RelayTransport.connect(
        str(relay_url),
        str(guid),
        password_reader(rec["id"], kernel, str(relay_url)),
        str(partner_guid),
        heartbeat=heartbeat,
        reconnect=reconnect,
        identity=_identity_from_record(rec),
        # The heartbeat becomes this connector's own registered work.
        spawn=lambda name, coro: kernel.ctx.tasks[
            kernel.ctx.spawn_task(rec["id"], name, coro)
        ]["_task"],
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
        "role": rec.get("role") or "kernel",
        "owner_guid": rec.get("owner_guid"),
        "exposes": rec.get("exposes") or [],
    }
