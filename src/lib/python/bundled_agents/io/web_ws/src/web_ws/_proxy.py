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

from starlette.websockets import WebSocketState
import logging
import secrets

from kernel import sender_context

from io_bridge import (
    decode_frame,
    encode_frame,
    find_bytes_path,
    gate_inbound,
    resolve_ingress,
    set_path,
)

logger = logging.getLogger(__name__)


def _make_client_id() -> str:
    return f"_ws_{secrets.token_hex(3)}"


# ─── binary frame codec ─────────────────────────────────────────
# The codec is shared with every other transport — it lives in `io_bridge._codec`.
# These module-level aliases keep the historical proxy-local names (used by tests +
# the read loop below). A WS text frame ⇒ `encode_frame` returns (utf-8 json, False)
# and we send it as text; bytes anywhere ⇒ (binary frame, True) sent as a binary frame.
_find_bytes_path = find_bytes_path
_set_path = set_path
encode_outbound = encode_frame
decode_inbound = decode_frame


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
    # AUTH GATE for the connect-time convenience auto-watch. Only mirror the
    # connected host's inbox if THIS leg's ingress rule permits an inbound `watch`
    # with NO credential (i.e. an open `allow_all` leg). On a sealed (`deny_inbound`)
    # or credentialed (`password`) leg we do NOT auto-watch — otherwise a client that
    # merely completes the WS handshake would passively receive the host's events
    # without ever passing the gate (the seal bypass). A client on a gated leg must
    # send an explicit `watch` frame, which IS gated per-frame (and can carry a token
    # on the envelope). Resolved here at connect; explicit frames re-resolve live.
    if gate_inbound(
        resolve_ingress(kernel.get(web_agent_id) or {}), "watch", {"src": host_agent_id}
    ).allowed:
        kernel.watch(host_agent_id, client_id)
        watching = {host_agent_id}
    else:
        watching = set()

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

    # ─── auth gate (the io_bridge ingress rule on THIS leg) ─────
    # This WS face is a CHANNEL; the leg's rule lives on the web_ws agent's
    # own record (`web_agent_id`). Resolved PER FRAME so opening the leg
    # (`update_agent ingress_rule=…`) takes effect live, without a reconnect.
    # Absent rule ⇒ DenyInbound (SEALED by default — a bare leg refuses every
    # inbound frame; open it consciously with `ingress_rule=allow_all`). A sealed
    # leg denies AND teaches how to open, via the reply's `{reason, hint, see}` —
    # discovery-through-denial. The gate itself
    # is the shared `gate_inbound` (same extract+authorize path as the bridge
    # engine read loop) — message modality ⇒ the default EnvelopeExtractor.
    def _gate(kind: str, frame: dict):
        rec = kernel.get(web_agent_id) or {}
        return gate_inbound(resolve_ingress(rec), kind, frame)

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
        decision = _gate("call", frame)
        if not decision.allowed:
            # Teaching denial: resolve the caller's promise with reason + the
            # pointer to the readme that explains how to open the edge.
            data = {"error": decision.reason, "reason": "unauthorized"}
            if decision.hint:
                data["hint"] = decision.hint
            if decision.see:
                data["see"] = decision.see
            await _send_envelope({"type": "reply", "id": fid, "data": data})
            return
        try:
            with sender_context(web_agent_id):
                reply = await kernel.send(target, payload)
            await _send_envelope({"type": "reply", "id": fid, "data": reply})
        except Exception as e:
            await _send_envelope({"type": "error", "id": fid, "error": str(e)})

    async def _on_emit(frame):
        if not _gate("emit", frame).allowed:
            return
        with sender_context(web_agent_id):
            await kernel.emit(frame.get("target", ""), frame.get("payload", {}))

    async def _on_watch(frame):
        if not _gate("watch", frame).allowed:
            return
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
        if not _gate("state_subscribe", frame).allowed:
            return  # sealed leg leaks no telemetry, not just no dispatch
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
