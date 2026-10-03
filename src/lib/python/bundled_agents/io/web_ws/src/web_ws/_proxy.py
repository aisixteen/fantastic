"""WebSocket proxy — translates browser frames to/from kernel.send.

Two on-the-wire formats, picked automatically by payload content:

  TEXT FRAMES (JSON, default):
    C->S {type:"call",   target, payload, id, secret?}
                                                  -> kernel.send -> reply
                                                     (`secret` = the item's marks —
                                                     dotted strings and/or arrays
                                                     of steps, `kernel/_secret`)
    C->S {type:"emit",   target, payload}         -> kernel.emit (no reply)
    C->S {type:"watch",  src}                     -> mirror src's inbox into ours
    C->S {type:"unwatch", src}
    S->C {type:"reply",  id, data}                -> reply for a prior call
    S->C {type:"error",  id, error}               -> reply error
    S->C {type:"event",  src, payload}            -> an event from a watched agent
                                                     (`src` = the name the client watched)
    S->C {type:"event",  payload}                 -> an event emitted to THIS connection
    S->C {type:"emit",   target, payload, caller?} -> the host emitting on a page
                                                     agent (`<guid>|<agent>`: every
                                                     tab of that guid)
    S->C {type:"reply_mirror", id, data}          -> a reply another tab of THIS
                                                     connection's hello guid got

  BINARY FRAMES (when payload contains a `bytes` value anywhere):
    [4-byte BE uint32 H | H-byte JSON envelope | M-byte raw blob]
    Envelope JSON has the bytes value replaced by null and an extra
    field `_binary_path`: the steps to it from the envelope root, a
    JSON array — a key as a string (taken literally, so
    `["payload", "a.b"]` is the key "a.b"), a list index as an int.
    Receiver parses header, reads remaining bytes as the binary body,
    sets the body at `_binary_path` in the envelope, then handles it as
    a regular text-frame envelope.

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
from kernel._kernel import PeerRoute
from kernel._refusal import TRANSPORT_ERROR, UNKNOWN_AGENT, bad, refusal

from io_bridge import (
    decode_frame,
    encode_frame,
    gate_inbound,
    resolve_ingress,
)

logger = logging.getLogger(__name__)


# hello guid → its live connections, per kernel: (id(kernel.ctx), guid) →
# {client_id: that connection's `_send_envelope`}. Every reply a leg answers to
# one tab of an identity is mirrored to the others (owner, 2026-10-03).
_TABS: dict[tuple[int, str], dict] = {}


def _make_client_id() -> str:
    return f"_ws_{secrets.token_hex(3)}"


def _name(frame: dict, key: str) -> str:
    """A frame field the proxy USES as a name (`type`, `target`, `src`, an answer's
    `id`): the field when it is a string, else `""` — as rust and swift read it. A
    list or object here was hashed (`src in watching`, `out_calls.get(id)`, the
    handler table, the kernel's agent table) and its TypeError ended the socket,
    every in-flight call on it included (audit W-9, 2026-10-03)."""
    v = frame.get(key)
    return v if isinstance(v, str) else ""


# ─── binary frame codec ─────────────────────────────────────────
# The codec is shared with every other transport — it lives in `io_bridge._codec`.
# A WS text frame ⇒ `encode_frame` returns (utf-8 json, False) and is sent as text;
# bytes anywhere ⇒ (binary frame, True), sent as a binary frame.


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
    kernel.ctx.ensure_inbox(client_id)
    # AUTH GATE for the connect-time convenience auto-watch. Only mirror the
    # connected host's inbox if THIS leg's ingress rule permits an inbound `watch`
    # with NO credential (i.e. an open `allow_all` leg). On a sealed (`deny_inbound`)
    # or credentialed (`password`) leg we do NOT auto-watch — otherwise a client that
    # merely completes the WS handshake would passively receive the host's events
    # without ever passing the gate (the seal bypass). A client on a gated leg must
    # send an explicit `watch` frame, which IS gated per-frame (and can carry a token
    # on the envelope). Resolved here at connect; explicit frames re-resolve live.
    auto_watch = gate_inbound(
        resolve_ingress(kernel.get(web_agent_id) or {}), "watch", {"src": host_agent_id}
    ).allowed
    # name the client used → (the RESOLVED agent id watched, that watch's own
    # reader, its drain). Only a watch that REALLY registered is recorded here
    # (see `_watch`). ONE READER PER WATCH, so every mirrored event goes out
    # stamped with the `src` it came from — a bridge on the other end re-emits
    # only what it subscribed to.
    watching: dict[str, tuple[str, str, asyncio.Task]] = {}

    async def drain_outbound(reader: str, src: str | None) -> None:
        q = kernel.ctx.ensure_inbox(reader)
        while True:
            payload = await q.get()
            if ws.application_state != WebSocketState.CONNECTED:
                return
            envelope = (
                {"type": "event", "payload": payload}
                if src is None
                else {"type": "event", "src": src, "payload": payload}
            )
            frame, is_bin = encode_frame(envelope)
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

    # REGISTERED: this loop lives as long as the socket does.
    drain_task = kernel.ctx.tasks[
        kernel.ctx.spawn_task(
            host_agent_id, "ws_drain", drain_outbound(client_id, None)
        )
    ]["_task"]

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
        frame, is_bin = encode_frame(envelope)
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
    # inbound frame; open it consciously with `ingress_rule=allow_all_ingress`). A sealed
    # leg denies AND teaches how to open, via the reply's `{reason, hint, see}` —
    # discovery-through-denial. The gate itself
    # is the shared `gate_inbound` (same extract+authorize path as the bridge
    # engine read loop) — message modality ⇒ the default EnvelopeExtractor.
    def _gate(kind: str, frame: dict):
        rec = kernel.get(web_agent_id) or {}
        return gate_inbound(resolve_ingress(rec), kind, frame)

    # ─── inbound frame handlers ─────────────────────────────────
    # Browser-originated traffic has no agent identity of its own: every
    # dispatch is tagged, explicitly, with `web_agent_id` (this surface's
    # own id) — it is gated as that agent and every external call/emit
    # originates from it.
    async def _on_call(frame):
        target = _name(frame, "target")
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
            await _answer(fid, data)
            return
        # A handler raise settles as a failed outcome (`{"error": ..., "raised":
        # True}` in result) — send never leaks it, so the old except net is dead.
        #
        # ONE ID SPACE, wire included: the frame `id` correlates the reply AND
        # names the execution (the ledger's dedup/join/retry key). Reusing an
        # id DECLARES "same intent" — the recorded outcome returns, nothing
        # re-fires (double-click, racing tabs, retries). Mint a fresh id per
        # intent; ":" is refused (minted namespace).
        item = {"target": target, "payload": payload}
        if isinstance(fid, str) and fid:
            item["id"] = fid
        # SECRET MARKS cross the leg verbatim (`kernel/_secret`): this kernel
        # records the call as shown, exactly as for a local caller — the kernel's
        # own check refuses a malformed mark before anything is dispatched.
        if "secret" in frame:
            item["secret"] = frame["secret"]
        with sender_context(web_agent_id):
            reply = (await kernel.send([item]))[0]["result"]
        await _answer(fid, reply)

    async def _answer(fid, data) -> None:
        """Reply to THIS connection, then mirror the reply to every OTHER live
        connection that said hello with this one's guid."""
        await _send_envelope({"type": "reply", "id": fid, "data": data})
        if peer["guid"] is None:
            return
        # TODO (owner, open): DESYNC between the tabs of one identity is envisioned
        # and unsolved — a mirror shows the reply, it does not reconcile state the
        # other tabs built differently. Think later.
        mirror = {"type": "reply_mirror", "id": fid, "data": data}
        tabs = _TABS.get((id(kernel.ctx), peer["guid"]), {})
        for cid, send in list(tabs.items()):
            if cid != client_id:
                await send(mirror)

    async def _on_emit(frame):
        if not _gate("emit", frame).allowed:
            return
        with sender_context(web_agent_id):
            await kernel.emit(_name(frame, "target"), frame.get("payload", {}))

    async def _watch(src: str, frame_id: str) -> None:
        """THE ONE WATCH PATH — the connect-time auto-watch and an explicit
        `watch` frame both come here. FAIL LOUD: a name that resolves to no agent,
        or a gate refusal, is answered with an `error` frame and NOT recorded as
        watched. (The connect-time watch used to discard the result and mark the
        name watched anyway; a name the kernel's `watch` did not resolve mirrored
        nothing, said nothing, and the explicit watch that followed was then
        skipped as a duplicate: measured, a page got zero events.)

        The name resolves the way `send` resolves a target (`kernel` or a literal
        id). UNDER THE LEG'S OWN IDENTITY, so the kernel's gate
        sees a real caller: without it `_current_sender` is None, the kernel reads
        the watch as an OPERATOR's, and the two-sided gate never runs — which is
        exactly how a leg granted only `assistant:[send]` could mirror any
        agent's inbox and read the model's prompt out of it."""
        if not src or src in watching:
            return
        target = kernel._resolve_agent(src)
        if target is None:
            await _send_envelope(
                {
                    "type": "error",
                    "id": frame_id,
                    **refusal(
                        UNKNOWN_AGENT,
                        f"watch: no agent {src!r} (an id, or `kernel`)",
                        src=src,
                    ),
                }
            )
            return
        reader = f"{client_id}>{target.id}"
        kernel.ctx.ensure_inbox(reader)
        with sender_context(web_agent_id):
            denial = kernel.watch(target.id, reader)
        if denial is not None:
            kernel.ctx.inboxes.pop(reader, None)
            await _send_envelope({"type": "error", "id": frame_id, **denial})
            return
        drain = kernel.ctx.tasks[
            kernel.ctx.spawn_task(
                host_agent_id, "ws_drain", drain_outbound(reader, src)
            )
        ]["_task"]
        watching[src] = (target.id, reader, drain)

    def _drop_watch(src: str) -> None:
        aid, reader, drain = watching.pop(src)
        kernel.unwatch(aid, reader)
        drain.cancel()
        kernel.ctx.inboxes.pop(reader, None)

    async def _on_watch(frame):
        if not _gate("watch", frame).allowed:
            return
        await _watch(_name(frame, "src"), frame.get("id", ""))

    async def _on_unwatch(frame):
        src = _name(frame, "src")
        if src in watching:
            _drop_watch(src)

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
        # The state stream is a KERNEL-WIDE read: every send's sender, target,
        # verb and payload summary, for every agent in the tree. So it is gated
        # on the ROOT exactly as a kernel verb is — the leg needs an explicit
        # `state_subscribe` edge, not merely an open ingress rule. An ingress
        # rule answers "may this client reach this leg"; it says nothing about
        # what the client may then READ. UNDER THE LEG'S OWN IDENTITY, like every
        # other handler here: with no sender the kernel reads an OPERATOR and the
        # gate never runs — a refactor of the ambient context (or a transport that
        # sets none) would turn the stream into an open tap (audit ARCH2-7).
        with sender_context(web_agent_id):
            denial = kernel.authorize_state_subscribe(web_agent_id)
        if denial is not None:
            await _send_envelope({"type": "error", "id": frame.get("id", ""), **denial})
            return
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

        # Conversation rows are not on this stream — an AI agent announces its own
        # rows to its watchers (`watch` on that agent, gated there).
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

    # ─── the PEER half: calls the host makes INTO the page ──────────
    # A page's kernel says who it is (`hello {guid}`); from then on the host
    # kernel can address its agents as `<guid>|<agent>` (kernel `peers`), and
    # every such call rides THIS socket as an outbound `call` frame, answered by
    # an inbound `reply`/`error` with the same id. This leg only carries the
    # frames — the host kernel routes and gates, the page kernel gates its own
    # agents. One guid per connection, pinned by the first hello; many connections
    # per guid (every tab of one identity).
    peer: dict = {"guid": None, "route": None, "closed": False}
    out_calls: dict[str, asyncio.Future] = {}
    out_seq = [0]

    async def _call_page(
        agent_id: str, payload: dict, caller: str, secret: tuple
    ) -> dict:
        # A call that resolved this route just as the socket closed must not wait
        # on a future the teardown has already swept — it settles now, the same
        # way the teardown settles the ones already waiting.
        if peer["closed"] or ws.application_state != WebSocketState.CONNECTED:
            return refusal(
                TRANSPORT_ERROR,
                f"the page {peer['guid']} disconnected before answering",
            )
        out_seq[0] += 1
        fid = f"h{out_seq[0]}"
        fut: asyncio.Future = asyncio.get_running_loop().create_future()
        out_calls[fid] = fut
        frame = {
            "type": "call",
            "id": fid,
            "target": agent_id,
            "payload": payload,
            "caller": caller,
        }
        # the item's marks, so the PAGE kernel masks its records of this call too
        if secret:
            frame["secret"] = list(secret)
        try:
            await _send_envelope(frame)
            return await fut
        finally:
            out_calls.pop(fid, None)

    async def _emit_page(agent_id: str, payload: dict, caller: str | None) -> None:
        frame = {"type": "emit", "target": agent_id, "payload": payload}
        if caller is not None:
            frame["caller"] = caller
        await _send_envelope(frame)

    async def _on_hello(frame):
        guid = frame.get("guid")
        if not isinstance(guid, str) or not guid or "|" in guid:
            await _send_envelope(
                {
                    "type": "error",
                    "id": frame.get("id", ""),
                    **bad(
                        "hello",
                        "guid",
                        "the page kernel's guid, a non-empty id without `|`",
                    ),
                }
            )
            return
        if not _gate("hello", frame).allowed:
            return  # a sealed leg carries no peer either; the calls it refuses teach
        if peer["guid"] is not None:
            if peer["guid"] != guid:
                await _send_envelope(
                    {
                        "type": "error",
                        "id": frame.get("id", ""),
                        **bad(
                            "hello",
                            "guid",
                            f"{peer['guid']} (pinned by this connection's first hello)",
                        ),
                    }
                )
            return
        # A guid already live on another connection is another tab of that
        # identity: this route JOINS it (one principal, kernel `register_peer`).
        route = PeerRoute(via=web_agent_id, call=_call_page, emit=_emit_page)
        kernel.ctx.register_peer(guid, route)
        peer["guid"], peer["route"] = guid, route
        _TABS.setdefault((id(kernel.ctx), guid), {})[client_id] = _send_envelope

    def _on_answer(frame):
        """An inbound `reply`/`error` answers one of OUR outbound calls — its
        data is that call's outcome (a page-side refusal included)."""
        fut = out_calls.get(_name(frame, "id"))
        if fut is not None and not fut.done():
            data = frame.get("data")
            fut.set_result(data if isinstance(data, dict) else {"result": data})

    FRAME_HANDLERS_INLINE = {
        "emit": _on_emit,
        "watch": _on_watch,
        "unwatch": _on_unwatch,
        "state_subscribe": _on_state_subscribe,
        "state_unsubscribe": _on_state_unsubscribe,
    }

    try:
        # THE CONNECT-TIME AUTO-WATCH of the endpoint the socket named — through
        # the one watch path, so a refusal is an `error` frame, never silence.
        if auto_watch:
            await _watch(host_agent_id, "")
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
            # A FRAME IS AN INPUT, NOT A CONTRACT. Decoding used to run bare: a
            # 3-byte binary frame raised `struct.error` out of the loop, a text
            # frame that parsed to `[1]` raised `AttributeError` on `.get` — and
            # either killed the whole connection through `finally`, cancelling
            # every in-flight `call` on it (an LLM generation with it). Answer the
            # bad frame and KEEP THE SOCKET.
            try:
                frame = decode_frame(raw)
            except Exception:
                frame = None
            if not isinstance(frame, dict):
                await _send_envelope(
                    {
                        "type": "error",
                        **bad(
                            "frame",
                            "frame",
                            "a well-formed call/emit/watch envelope",
                        ),
                    }
                )
                continue
            ftype = _name(frame, "type")
            if ftype == "call":
                # REGISTERED: `_on_call` DISPATCHES — it is real work, and a
                # bare task left it out of `ps` and running under whatever
                # scope opened the socket.
                t = kernel.ctx.tasks[
                    kernel.ctx.spawn_task(host_agent_id, "ws_call", _on_call(frame))
                ]["_task"]
                pending.add(t)
                t.add_done_callback(pending.discard)
            elif ftype == "hello":
                await _on_hello(frame)
            elif ftype in ("reply", "error"):
                _on_answer(frame)
            else:
                inline = FRAME_HANDLERS_INLINE.get(ftype)
                if inline is not None:
                    await inline(frame)
            # Unknown frame types are silently dropped (forward-compat).
    finally:
        # The page is gone: it is no longer an address, and every call still
        # waiting on it settles NOW with the reason — never a hang, never a retry.
        peer["closed"] = True
        if peer["route"] is not None:
            kernel.ctx.unregister_peer(peer["guid"], peer["route"])
            key = (id(kernel.ctx), peer["guid"])
            tabs = _TABS.get(key, {})
            tabs.pop(client_id, None)
            if not tabs:
                _TABS.pop(key, None)
        for fut in list(out_calls.values()):
            if not fut.done():
                fut.set_result(
                    refusal(
                        TRANSPORT_ERROR,
                        f"the page {peer['guid']} disconnected before answering",
                    )
                )
        drain_task.cancel()
        # Cancel any in-flight `call` handlers so the underlying
        # kernel.send tasks unwind (ollama_backend honors
        # CancelledError, releases its FIFO lock, emits done).
        for t in list(pending):
            t.cancel()
        if pending:
            await asyncio.gather(*pending, return_exceptions=True)
        for src in list(watching):
            _drop_watch(src)
        # Unregister any state-stream callbacks tied to this WS.
        for fn in state_unsubs:
            fn()
        state_unsubs.clear()
        kernel.ctx.inboxes.pop(client_id, None)
