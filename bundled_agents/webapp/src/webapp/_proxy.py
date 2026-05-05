"""WebSocket proxy — translates browser frames to/from kernel.send.

Two on-the-wire formats, picked automatically by payload content:

  TEXT FRAMES (JSON, default):
    C->S {type:"call",   target, payload, id}     -> kernel.send -> reply
    C->S {type:"emit",   target, payload}         -> kernel.emit (no reply)
    C->S {type:"watch",  src}                     -> mirror src's inbox into ours
    C->S {type:"unwatch", src}
    S->C {type:"reply",  id, data}                -> reply for a prior call
    S->C {type:"error",  id, error}               -> reply error
    S->C {type:"event",  payload}                 -> any event mirrored from kernel inbox

  BINARY FRAMES (when payload contains a `bytes` value anywhere):
    [4-byte BE uint32 H | H-byte JSON envelope | M-byte raw blob]
    Envelope JSON has the bytes value replaced by null and an extra
    field `_binary_path: "<dotted.path.to.bytes.field>"`. Receiver
    parses header, reads remaining bytes as the binary body, sets the
    body at `_binary_path` in the envelope, then handles it as a
    regular text-frame envelope.

The binary path eliminates base64 + JSON encoding for byte-heavy
payloads (audio, image, etc.). It's transparent to bundle handlers —
they receive `bytes` regardless of which wire format was used.
"""

from __future__ import annotations

import asyncio
import copy

from starlette.websockets import WebSocketState
import json
import logging
import secrets
import struct
from typing import Any

from kernel import _current_sender

logger = logging.getLogger(__name__)


def _make_client_id() -> str:
    return f"_ws_{secrets.token_hex(3)}"


# ─── binary frame helpers ───────────────────────────────────────


def _find_bytes_path(obj: Any, prefix: str = "") -> tuple[str, bytes] | None:
    """Walk obj; return (dotted_path, value) of first bytes value, or None."""
    if isinstance(obj, bytes):
        return (prefix, obj)
    if isinstance(obj, dict):
        for k, v in obj.items():
            p = f"{prefix}.{k}" if prefix else k
            r = _find_bytes_path(v, p)
            if r is not None:
                return r
    elif isinstance(obj, list):
        for i, v in enumerate(obj):
            p = f"{prefix}.{i}" if prefix else str(i)
            r = _find_bytes_path(v, p)
            if r is not None:
                return r
    return None


def _set_path(obj: Any, path: str, value: Any) -> None:
    parts = path.split(".")
    for p in parts[:-1]:
        obj = obj[int(p)] if isinstance(obj, list) else obj[p]
    last = parts[-1]
    if isinstance(obj, list):
        obj[int(last)] = value
    else:
        obj[last] = value


def encode_outbound(envelope: dict) -> tuple[bytes, bool]:
    """Serialize an envelope for the wire.

    Returns (frame_data, is_binary). If any bytes value is found, returns
    a binary frame `[4-byte BE length | header JSON | body bytes]`.
    Otherwise returns UTF-8-encoded JSON text.
    """
    found = _find_bytes_path(envelope)
    if found is None:
        return json.dumps(envelope, default=str).encode("utf-8"), False
    path, body = found
    head_obj = copy.deepcopy(envelope)
    _set_path(head_obj, path, None)
    head_obj["_binary_path"] = path
    head_bytes = json.dumps(head_obj, default=str).encode("utf-8")
    return struct.pack(">I", len(head_bytes)) + head_bytes + body, True


def decode_inbound(data: bytes | str) -> dict:
    """Parse a wire frame back into an envelope dict.

    Text frames: JSON-decoded. Binary frames: reconstructed envelope
    with bytes body restored at `_binary_path`.
    """
    if isinstance(data, str):
        return json.loads(data)
    # Binary frame
    head_len = struct.unpack(">I", data[:4])[0]
    head = json.loads(data[4 : 4 + head_len].decode("utf-8"))
    body = data[4 + head_len :]
    path = head.pop("_binary_path", None)
    if path is not None:
        _set_path(head, path, body)
    return head


# ─── proxy loop ─────────────────────────────────────────────────


async def run(ws, kernel, host_agent_id: str, web_agent_id: str) -> None:
    """Pump WS frames between a browser and the kernel.

    `host_agent_id` is the URL endpoint the browser connected to —
    every connection auto-watches that id so the browser sees its
    inbox events without an explicit `watch` frame.

    `web_agent_id` is THIS webapp's own agent id — used as the
    `_current_sender` for browser-driven calls/emits so telemetry
    rays originate visually from the webapp sprite (browsers don't
    have agent identity of their own).
    """
    """Drive one WebSocket connection."""
    client_id = _make_client_id()
    kernel._ensure_inbox(client_id)
    kernel.watch(host_agent_id, client_id)
    watching = {host_agent_id}

    async def drain_outbound() -> None:
        q = kernel._ensure_inbox(client_id)
        while True:
            payload = await q.get()
            if ws.application_state != WebSocketState.CONNECTED:
                return
            frame, is_bin = encode_outbound({"type": "event", "payload": payload})
            try:
                if is_bin:
                    await ws.send_bytes(frame)
                else:
                    await ws.send_text(frame.decode("utf-8"))
            except RuntimeError as e:
                # Same close race as _send_envelope: WS may close
                # between the state check and the send. Narrow catch.
                if "close message has been sent" not in str(e):
                    raise
                return

    drain_task = asyncio.create_task(drain_outbound())

    async def _send_envelope(envelope: dict) -> None:
        # Browser tab can close mid-call: an ollama `send` holds the WS
        # for tens of seconds while tokens stream; refreshing in that
        # window is normal. Two-step guard:
        #   1) check ws.application_state up front and skip if not
        #      CONNECTED — common case, no exception involved
        #   2) the close can still race in between the check and the
        #      actual send; narrow that to the ONE specific RuntimeError
        #      starlette raises ("Cannot call ... once a close message
        #      has been sent"). Any other RuntimeError is a real bug
        #      and propagates.
        if ws.application_state != WebSocketState.CONNECTED:
            return
        frame, is_bin = encode_outbound(envelope)
        try:
            if is_bin:
                await ws.send_bytes(frame)
            else:
                await ws.send_text(frame.decode("utf-8"))
        except RuntimeError as e:
            if "close message has been sent" not in str(e):
                raise

    # ─── inbound frame handlers ─────────────────────────────────
    # Browser-originated traffic has no agent context — the WS handler
    # runs outside any handler dispatch, so without help `_current_sender`
    # would be None and telemetry rays would have nowhere to start. Tag
    # the dispatch with `host_agent_id` (this webapp's own id) so every
    # external call/emit visually originates from the webapp sprite.
    async def _on_call(frame):
        target = frame.get("target", "")
        payload = frame.get("payload", {})
        fid = frame.get("id", "")
        token = _current_sender.set(web_agent_id)
        try:
            reply = await kernel.send(target, payload)
            await _send_envelope({"type": "reply", "id": fid, "data": reply})
        except Exception as e:
            await _send_envelope({"type": "error", "id": fid, "error": str(e)})
        finally:
            _current_sender.reset(token)

    async def _on_emit(frame):
        token = _current_sender.set(web_agent_id)
        try:
            await kernel.emit(frame.get("target", ""), frame.get("payload", {}))
        finally:
            _current_sender.reset(token)

    async def _on_watch(frame):
        src = frame.get("src", "")
        if src and src not in watching:
            kernel.watch(src, client_id)
            watching.add(src)

    async def _on_unwatch(frame):
        src = frame.get("src", "")
        if src in watching:
            kernel.unwatch(src, client_id)
            watching.discard(src)

    # ─── state stream bridge ───────────────────────────────────
    # Mirror of the watch/unwatch lifecycle but for the kernel's
    # direct-callback telemetry tap (kernel._state_subscribers).
    # On `state_subscribe`, we send an immediate snapshot frame and
    # register a callback that pumps `state_event` frames over WS.
    # The callback uses asyncio.create_task because the kernel's
    # _notify_state is sync (called from inside _fanout / lifecycle
    # methods); we can't await directly from there.
    state_unsubs: list = []

    async def _on_state_subscribe(frame):
        await _send_envelope(
            {"type": "state_snapshot", "agents": kernel.state_snapshot()}
        )

        def cb(event: dict) -> None:
            out = {"type": "state_event", **event}
            # Lifecycle events carry `name`; traffic events carry
            # `backlog` only — fill name lazily so the browser renders
            # with a single shape.
            if "name" not in out:
                rec = kernel.get(event["agent_id"]) or {}
                out["name"] = rec.get("display_name") or event["agent_id"]
            try:
                asyncio.get_running_loop().create_task(_send_envelope(out))
            except RuntimeError:
                # No running loop — this fanout fired during shutdown.
                # Drop silently; the WS is going down anyway.
                pass

        unsub = kernel.add_state_subscriber(cb)
        state_unsubs.append(unsub)

    async def _on_state_unsubscribe(frame):
        for fn in state_unsubs:
            fn()
        state_unsubs.clear()

    # `call` handlers may await for tens of seconds (LLM streams).
    # Awaiting them inline blocks the receive loop, so a peer-close
    # only registers AFTER the generation completes — backend keeps
    # burning resources on a now-orphaned conversation. Dispatch
    # `call` as a task and track pending; cancel on disconnect so
    # `kernel.send` (and downstream e.g. ollama_backend._run) sees
    # CancelledError and releases the lock immediately. The other
    # frame types (emit/watch/unwatch) are local state mutations —
    # cheap, awaited inline.
    pending: set[asyncio.Task] = set()

    FRAME_HANDLERS_INLINE = {
        "emit": _on_emit,
        "watch": _on_watch,
        "unwatch": _on_unwatch,
        "state_subscribe": _on_state_subscribe,
        "state_unsubscribe": _on_state_unsubscribe,
    }

    try:
        while True:
            try:
                msg = await ws.receive()
            except Exception:
                break
            # FastAPI/Starlette WebSocket.receive returns a dict with
            # 'type', 'text' or 'bytes'. Normalize.
            if msg.get("type") == "websocket.disconnect":
                break
            raw = msg.get("text")
            if raw is None:
                raw = msg.get("bytes")
            if raw is None:
                continue
            frame = decode_inbound(raw)
            ftype = frame.get("type")
            if ftype == "call":
                t = asyncio.create_task(_on_call(frame))
                pending.add(t)
                t.add_done_callback(pending.discard)
            else:
                inline = FRAME_HANDLERS_INLINE.get(ftype)
                if inline is not None:
                    await inline(frame)
            # Unknown frame types are silently dropped (forward-compat).
    finally:
        drain_task.cancel()
        # Cancel any in-flight `call` handlers so the underlying
        # kernel.send tasks unwind (ollama_backend honors
        # CancelledError, releases its FIFO lock, emits done).
        for t in list(pending):
            t.cancel()
        if pending:
            await asyncio.gather(*pending, return_exceptions=True)
        for src in watching:
            kernel.unwatch(src, client_id)
        # Unregister any state-stream callbacks tied to this WS.
        for fn in state_unsubs:
            fn()
        state_unsubs.clear()
        kernel._inboxes.pop(client_id, None)
