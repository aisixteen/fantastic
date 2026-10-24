"""io_bridge engine — the transport-agnostic bridge engine.

Every bridge bundle is the SAME machine: a per-agent state holding a transport, a
read loop that turns inbound `call`/`reply`/`error`/`event` frames into
`kernel.send`/`kernel.emit`/pending-future resolution, and the verbs
`boot`/`reconnect`/`forward`/`watch_remote`/`unwatch_remote`/`reflect`. The ONLY
thing that differs per bundle is *which transport* `boot` builds — supplied as the
`build_transport` seam — plus a `sentence` + `reflect_fields` for reflect dressing.

A bundle's `tools.py` is then ~15 lines:

    from io_bridge import make_verbs, dispatch, on_delete
    from <bundle>._build import build_transport, SENTENCE, reflect_fields, README
    VERBS = make_verbs(build_transport=build_transport, sentence=SENTENCE,
                       reflect_fields=reflect_fields, default_kind="<kind>")
    async def handler(id, payload, kernel): return await dispatch(VERBS, id, payload, kernel)
    async def on_delete(agent): return await on_delete(agent)
    readme = README

Reply correlation: `corr_id = f"{bridge_id}-{boot nonce}-{counter}"` — the bridge
id keeps bridges apart, the nonce (minted at every boot) keeps BOOTS apart, and the
frame `id` echoes it so logs correlate across hops. The frame id NAMES the execution
on the receiver, so an id reused by a restarted sender would join a settled
execution there and never run.

A leg's rules (`ingress_rule`/`egress_rule`/`auth`) are read off the LIVE record per
frame — never cached at boot — so what `reflect` reports is what the gate enforces
the moment `update_agent` writes it.
"""

from __future__ import annotations

import asyncio
import secrets
from dataclasses import dataclass, field
from typing import Any, Awaitable, Callable

from io_bridge._transport import ConnectionClosed, _BaseTransport
from io_bridge._base import Action, Decision, EgressRule, IngressRule, describe
from io_bridge.channel import CredentialExtractor, EnvelopeExtractor
from io_bridge.ingress_rules import resolve_ingress, unknown_ingress_refusal
from io_bridge.egress_rules import resolve_egress
from kernel import note_reply_secret
from kernel._refusal import NOT_CONNECTED, UNHANDLED, refusal

DEFAULT_FORWARD_TIMEOUT = 30.0

# The message-modality default: the credential rides the frame ENVELOPE
# (`auth_token`), never the dispatched payload. Every message channel (ws/cloud/
# memory bridge + the web_ws inbound leg) shares this one extractor instance.
_ENVELOPE = EnvelopeExtractor()


def gate_inbound(
    rule: IngressRule,
    kind: str,
    frame: dict,
    extractor: CredentialExtractor = _ENVELOPE,
) -> Decision:
    """THE single inbound choke point, shared by the engine read loop AND the web
    inbound legs (web_ws/web_rest). Pulls the peer's credential off the frame with
    the channel's `extractor`, then asks the leg's INGRESS rule to authorize the
    action. `kind` ∈ call|emit|watch|state_subscribe; for `call` the verb is the
    payload's `type`, otherwise the verb IS the kind. Returns the `Decision` (a
    denial carries the teaching `hint`/`see`)."""
    inner = frame.get("payload")
    inner = inner if isinstance(inner, dict) else {}
    verb = inner.get("type", "") if kind == "call" else kind
    addr = frame.get("target") or frame.get("src") or ""
    return rule.authorize(
        Action(kind, addr, verb, inner, token=extractor.extract(frame))
    )


def stamp_egress(rule: EgressRule, frame: dict) -> dict:
    """The outbound DECORATOR, shared by every dialing leg. Stamps the leg's EGRESS
    credential on the envelope if its rule presents one (`password` ⇒ the group
    token; `silent` ⇒ None ⇒ no field, wire unchanged). The dispatched payload stays
    clean — the target never sees the token."""
    tok = rule.credential()
    if tok is not None:
        frame["auth_token"] = tok
    return frame


# `async build_transport(kind, rec, kernel, st) -> _BaseTransport`. Raises on bad
# config (the engine wraps it into a `{"error": ...}` boot reply). May set
# `st.cleanup` for transport-specific teardown.
BuildTransport = Callable[[str, dict, Any, "_BridgeState"], Awaitable[_BaseTransport]]


@dataclass
class _BridgeState:
    transport: _BaseTransport | None = None
    # Empty until `boot` succeeds — reflect falls through to the record's
    # `transport` field for the advertised config.
    transport_kind: str = ""
    read_task: asyncio.Task | None = None
    pending: dict[str, asyncio.Future] = field(default_factory=dict)
    # In-flight INBOUND dispatches (one task per `call` frame). Held so the read
    # loop can keep reading while a handler runs, and so teardown can cancel them.
    pending_calls: set[asyncio.Task] = field(default_factory=set)
    # `<bridge id>-<nonce>`, minted at every boot, and the count under it: the
    # frame ids of this boot's forwards (`_next_corr`).
    corr_prefix: str = ""
    corr_counter: int = 0
    # Optional transport-specific teardown the bundle registers in build_transport.
    # AWAITED by the engine on teardown — anything blocking belongs on a thread,
    # not on the loop.
    cleanup: Callable[[], Awaitable[None]] | None = None
    # Bundle-specific reflect/diagnostic state. The engine only clears it on
    # teardown; the bundle's `reflect_fields` reads it.
    extra: dict = field(default_factory=dict)
    # The remote agents this leg SUBSCRIBED to (`watch_remote`). An inbound
    # `event` frame is re-emitted only when its `src` is here — nothing flows
    # until subscribed, and nothing after an unwatch.
    watching: set[str] = field(default_factory=set)


# Per-agent bridge state — process-memory only, keyed by agent id (shared across
# all bridge bundles; ids are unique). Lost on restart, which is correct: a fresh
# kernel re-boots bridges via their `boot` verb.
_bridges: dict[str, _BridgeState] = {}

# Test seam: MemoryTransport tests inject a pre-built transport here before booting
# (a memory pair can't be carried in agent.yaml). `boot` pops it for `transport=memory`.
_test_transport_inject: dict[str, _BaseTransport] = {}


# ─── helpers ────────────────────────────────────────────────────


def _state(id: str) -> _BridgeState:
    s = _bridges.get(id)
    if s is None:
        s = _BridgeState()
        _bridges[id] = s
    return s


def _new_corr_prefix(id: str) -> str:
    """`<bridge id>-<12 hex>`: a fresh nonce per BOOT (ts's `mintPrefix` width).
    The counter under it restarts at every boot, so without the nonce a
    restarted sender's `<leg>-1` would name an execution the receiver had already
    settled — the new call would be joined to it and never run."""
    return f"{id}-{secrets.token_hex(6)}"


def _next_corr(st: _BridgeState) -> str:
    st.corr_counter += 1
    # DASH, not colon (kept): the wire's frame id is the execution's NAME; a ":"
    # here would land in the kernel-minted pid namespace and be refused on
    # the serving side. Bridge-minted correlators are caller-chosen ids like
    # any other — fresh per forward AND per boot, so each forward is its own
    # intent.
    return f"{st.corr_prefix}-{st.corr_counter}"


def validate_meta(meta: dict, record: dict) -> str | None:
    """Write-time check of the leg's `ingress_rule` (`unknown_ingress_refusal`).
    Re-exported by every bridge bundle."""
    return unknown_ingress_refusal(meta)


def forward_payload_secret(payload: Any) -> list[str]:
    """The secret paths of a FORWARDED call (the module-level `payload_secret` hook
    of `ws_bridge` and `relay_connector`): a `forward` of a `set` marks each
    `payload.fields.<name>`; a name that is empty or holds a dot marks the whole
    `payload.fields`. The caller cannot know the far bundle before sending, so this
    is deliberate over-redaction in the CALLER's records for every forwarded `set`:
    the far kernel's own record is exact. A `get`'s reply marks come back on the
    reply frame instead (`secret`). Mirrors the Rust `forward_payload_secret`."""
    if not isinstance(payload, dict) or payload.get("type") != "forward":
        return []
    inner = payload.get("payload")
    if not isinstance(inner, dict) or inner.get("type") != "set":
        return []
    fields = inner.get("fields")
    if not isinstance(fields, dict):
        return []
    if all(isinstance(k, str) and k and "." not in k for k in fields):
        return [f"payload.fields.{k}" for k in fields]
    return ["payload.fields"]


def _wire_marks(value: Any) -> list[str]:
    """A reply frame's optional `secret`, validated: a list of non-empty dotted
    paths (no empty segment, never `type`). Anything else is NO marks; the reply
    is still delivered."""
    if not isinstance(value, list):
        return []
    for m in value:
        if not (isinstance(m, str) and m and "" not in m.split(".") and m != "type"):
            return []
    return list(value)


async def _serve_inbound_call(
    st: "_BridgeState", transport: _BaseTransport, kernel: Any, frame: dict
) -> None:
    """Serve ONE inbound `call` frame and ship its reply.

    Run as its own task, never inline in the read loop: the local handler may
    itself `forward` back over this same bridge and await the answer, and a loop
    parked inside `kernel.send` cannot read the frame that would settle it — the
    round trip deadlocks until the 30s forward timeout. One task per call keeps
    the loop free to read; `st.pending_calls` holds them so teardown can cancel.

    The read loop is a task this leg's `boot` spawned, so this task is the
    leg's too: the inbound call is sent AS THE LEG and gated as the leg — its
    `can_call` and the target's `callable_by` decide. Whoever called on the other
    kernel is that kernel's business; here the caller is the leg.
    """
    target = frame.get("target")
    payload = frame.get("payload") or {}
    reply_marks: list[str] = []  # the reply's own secret paths, sent with it
    rec = kernel.get(kernel.id) or {}
    # AUTH GATE — the single shared choke point (`gate_inbound`, also used by the
    # web inbound legs). The leg's INGRESS rule — read off the LIVE record, per
    # frame, as every web/file/exec leg does — decides whether the peer may run
    # this inbound call locally; a denial still gets a `reply` (echoing the
    # corr-id) so the caller resolves cleanly.
    decision = gate_inbound(resolve_ingress(rec), "call", frame)
    if not decision.allowed:
        # Teaching denial — mirror the web inbound legs so discovery-through-
        # denial works over bridge legs too (reason + hint + see).
        reply: Any = {"error": decision.reason, "reason": "unauthorized"}
        if decision.hint:
            reply["hint"] = decision.hint
        if decision.see:
            reply["see"] = decision.see
    else:
        # A handler raise settles as a failed outcome — the error rides in
        # `result`, so the wire keeps its `data` shape.
        #
        # ONE ID SPACE, wire included: the frame's `id` correlates the reply AND
        # names the execution in the ledger. Reusing an id DECLARES "same
        # intent" — the recorded outcome returns and nothing re-fires
        # (double-click, racing tabs, retries). Mint a fresh id per intent; ":"
        # is refused (the minted namespace); a reused id aimed elsewhere refuses
        # loudly naming the bound target.
        item = {"target": target, "payload": payload}
        fid = frame.get("id")
        if isinstance(fid, str) and fid:
            item["id"] = fid
        out = (await kernel.send([item]))[0]
        reply = out["result"]
        reply_marks = list(out.get("secret") or [])
    frame_out: dict = {"type": "reply", "id": frame.get("id"), "data": reply}
    if reply_marks:
        frame_out["secret"] = reply_marks
    try:
        await transport.send(frame_out)
    except ConnectionClosed:
        pass  # the peer went away mid-dispatch; the read loop already knows


async def _read_loop(id: str, kernel: Any) -> None:
    """Long-lived consumer of the transport. Frame shapes:

      - inbound `call` — `{type:'call', id, target, payload}`: dispatch via
        `kernel.send([{"target", "payload", "id"}])` AS THE LEG, reply
        `{type:'reply', id, data}`. (For a
        pure-client WS bridge this never fires — its peer is `web_ws`; for paired
        transports — MemoryTransport tests, the relay — it carries the callee role.)
      - inbound `reply` — resolve the pending Future for `id`.
      - inbound `error` — `{type:'error', id, error}` from a remote dispatch that
        RAISED; fail the pending forward promptly instead of hanging to timeout.
      - inbound `event` — `{type:'event', src, payload}` from a remote `watch`;
        re-emitted (payload + `src`) on this bridge's inbox ONLY when `src` is a
        subscribed remote agent (`watch_remote`), else dropped.

    On ConnectionClosed: emit `{type:'bridge_down'}`, fail every pending Future.
    """
    st = _state(id)
    transport = st.transport
    assert transport is not None
    try:
        while True:
            try:
                frame = await transport.recv()
            except ConnectionClosed:
                break
            ftype = frame.get("type")
            if ftype == "call":
                # TRACKED TASK, not an inline await — see `_serve_inbound_call`.
                t = asyncio.create_task(
                    _serve_inbound_call(st, transport, kernel, frame)
                )
                st.pending_calls.add(t)
                t.add_done_callback(st.pending_calls.discard)
            elif ftype in ("reply", "error"):
                # Only a STRING id names one of our forwards (rust/swift skip the
                # rest). A list id was hashed by `pending.pop` and its TypeError
                # ended this loop while `reflect` still read `connected:true`.
                fid = frame.get("id")
                fut = st.pending.pop(fid, None) if isinstance(fid, str) else None
                if fut is not None and not fut.done():
                    if ftype == "reply":
                        # the reply's optional `secret` names dotted paths into
                        # its data (the far bundle's reply marks)
                        fut.set_result(
                            (frame.get("data"), _wire_marks(frame.get("secret")))
                        )
                    else:
                        fut.set_result(
                            ({"error": f"remote error: {frame.get('error')}"}, [])
                        )
            elif ftype == "event":
                # The `watch_remote` RETURN half: re-emitted on the bridge's OWN inbox
                # for local watchers ONLY when its `src` is a remote agent this leg
                # subscribed to (a relay leg's `watch_directory` subscribes its
                # directory, `src:"relay"`). An event nobody asked for is dropped.
                src = frame.get("src")
                payload = frame.get("payload")
                if (
                    isinstance(src, str)
                    and src in st.watching
                    and isinstance(payload, dict)
                ):
                    try:
                        await kernel.emit(id, {**payload, "src": src})
                    except Exception:
                        pass
            # Other frame types are ignored so the loop stays robust.
    finally:
        # The transport is gone — every in-flight inbound dispatch has nowhere to
        # send its reply. Cancel them so their handlers unwind now instead of
        # running to completion into a dead socket.
        for t in list(st.pending_calls):
            t.cancel()
        st.pending_calls.clear()
        st.watching.clear()
        try:
            await kernel.emit(id, {"type": "bridge_down"})
        except Exception:
            pass
        for _cid, fut in list(st.pending.items()):
            if not fut.done():
                fut.set_exception(ConnectionError("bridge transport closed"))
        st.pending.clear()


async def _teardown(id: str) -> None:
    """Cancel the read loop, close the transport, run transport cleanup, reject
    pending. Leaves state in a re-bootable shape (transport=None)."""
    st = _bridges.get(id)
    if st is None:
        return
    if st.read_task is not None and not st.read_task.done():
        st.read_task.cancel()
        try:
            await st.read_task
        except (asyncio.CancelledError, Exception):
            pass
    for t in list(st.pending_calls):
        t.cancel()
    st.pending_calls.clear()
    if st.transport is not None:
        try:
            await st.transport.close()
        except Exception:
            pass
    if st.cleanup is not None:
        try:
            await st.cleanup()
        except Exception:
            pass
    for fut in list(st.pending.values()):
        if not fut.done():
            fut.set_exception(ConnectionError("bridge shut down"))
    st.pending.clear()
    st.transport = None
    st.read_task = None
    st.cleanup = None
    st.extra.clear()
    st.watching.clear()


async def on_delete(agent: Any) -> None:
    """Cascade hook — tears the bridge down by agent id."""
    await _teardown(agent.id)


async def boot(
    id,
    payload,
    kernel,
    *,
    build_transport: BuildTransport,
):
    """Shared boot: idempotency + the `memory` test kind + the `build_transport`
    seam for every other kind, then the common tail (mint this boot's correlator
    nonce, spawn read loop, emit `bridge_up`). The leg's rules are NOT cached here:
    the gate and `forward` read them off the live record per frame; boot only
    refuses an egress rule the registry does not know, before dialing."""
    rec = kernel.get(id) or {}
    st = _state(id)
    if st.transport is not None and not st.transport.closed:
        return {"already": True, "transport": st.transport_kind}
    try:
        resolve_egress(rec)
    except ValueError as e:
        return {"error": f"bridge: bad auth rule: {e}"}

    kind = rec.get("transport") or "ws"
    transport: _BaseTransport
    if kind == "memory":
        if id not in _test_transport_inject:
            return {"error": "bridge: memory transport requires test injection"}
        transport = _test_transport_inject.pop(id)
    else:
        try:
            transport = await build_transport(kind, rec, kernel, st)
        except Exception as e:
            # cleanup may have been partially registered (e.g. tunnel opened then
            # ws failed) — run it so we don't leak.
            if st.cleanup is not None:
                try:
                    await st.cleanup()
                except Exception:
                    pass
                st.cleanup = None
            return {"error": f"bridge: {kind} boot failed: {e}"}

    st.transport = transport
    st.transport_kind = kind
    st.corr_prefix, st.corr_counter = _new_corr_prefix(id), 0
    # REGISTERED: the read loop outlives this `boot` dispatch and re-emits
    # every inbound frame for the life of the leg.
    st.read_task = kernel.ctx.tasks[
        kernel.ctx.spawn_task(id, "read_loop", _read_loop(id, kernel))
    ]["_task"]
    await kernel.emit(id, {"type": "bridge_up"})
    return {"booted": True, "transport": kind}


# ─── shared verbs (transport-agnostic) ──────────────────────────


async def _forward(id, payload, kernel):
    """args: target:str (req — an id on the REMOTE kernel), payload:dict (req — the call to run there, a verb frame with its own `type`), forward_timeout:float? (default 30s — not `timeout`, which is a call's own routing field). Example: {"type":"forward","target":"src","payload":{"type":"read_stream","path":"data.bin","offset":0}}. Ships a raw `{type:'call', id, target, payload}` frame over the transport, awaits the matching reply, returns the unwrapped data. The far kernel judges the call as the leg that serves it there. Local→local stays direct kernel.send (this verb is only for cross-kernel). Multi-hop loop detection is the caller's responsibility — the bridge ships whatever it's given."""
    target = payload.get("target")
    inner = payload.get("payload")
    if not target or not isinstance(inner, dict):
        return {"error": "bridge.forward: target (str) + payload (dict) required"}
    timeout = float(payload.get("forward_timeout", DEFAULT_FORWARD_TIMEOUT))

    st = _state(id)
    if st.transport is None or st.transport.closed:
        return refusal(
            NOT_CONNECTED, "bridge.forward: not connected", hint="call boot first"
        )

    # The leg's EGRESS rule, off the live record per forward (never a boot-time
    # copy): an operator's `update_agent egress_rule=…` stamps the next frame.
    try:
        egress = resolve_egress(kernel.get(id) or {})
    except ValueError as e:
        return {"error": f"bridge: bad auth rule: {e}"}
    corr = _next_corr(st)
    fut: asyncio.Future = asyncio.get_event_loop().create_future()
    st.pending[corr] = fut

    frame = {"type": "call", "id": corr, "target": target, "payload": inner}
    # Stamp this leg's EGRESS credential on the envelope via the shared decorator.
    frame = stamp_egress(egress, frame)
    try:
        await st.transport.send(frame)
    except ConnectionClosed as e:
        st.pending.pop(corr, None)
        return {"error": f"bridge.forward: send failed: {e}"}

    try:
        data, marks = await asyncio.wait_for(fut, timeout=timeout)
    except asyncio.TimeoutError:
        st.pending.pop(corr, None)
        return {"error": f"bridge.forward: timeout after {timeout}s"}
    except ConnectionError as e:
        return {"error": f"bridge.forward: {e}"}
    # The far reply's marks, rooted at this reply: the calling kernel records the
    # forward as `[secret]` there (this runs inside the forward's own dispatch);
    # the caller gets `data` whole.
    note_reply_secret(marks)
    return data


async def _watch_remote(id, payload, kernel):
    """args: target:str (req — id on the REMOTE kernel to watch). Sends `{type:'watch', src:<target>}` over the transport and subscribes this leg to `target`. Subsequent `{type:'event', src, payload}` frames whose `src` is subscribed are re-emitted on THIS bridge agent's inbox as the payload plus `src`; an event for anything not subscribed is dropped. Local watchers subscribe to the bridge with `kernel.watch(<bridge_id>, ...)`. Idempotent (web_ws de-dups via its own `watching` set)."""
    target = payload.get("target")
    if not target or not isinstance(target, str):
        return {"error": "bridge.watch_remote: target (str) required"}
    st = _state(id)
    if st.transport is None or st.transport.closed:
        return refusal(
            NOT_CONNECTED, "bridge.watch_remote: not connected", hint="call boot first"
        )
    try:
        await st.transport.send({"type": "watch", "src": target})
    except ConnectionClosed as e:
        return {"error": f"bridge.watch_remote: send failed: {e}"}
    st.watching.add(target)
    return {"ok": True, "watching": target}


async def _unwatch_remote(id, payload, kernel):
    """args: target:str (req — id previously passed to watch_remote). Unsubscribes this leg from `target` (events for it are dropped from now on) and sends `{type:'unwatch', src:<target>}` so the remote stops emitting them."""
    target = payload.get("target")
    if not target or not isinstance(target, str):
        return {"error": "bridge.unwatch_remote: target (str) required"}
    st = _state(id)
    if st.transport is None or st.transport.closed:
        return refusal(
            NOT_CONNECTED,
            "bridge.unwatch_remote: not connected",
            hint="call boot first",
        )
    st.watching.discard(target)
    try:
        await st.transport.send({"type": "unwatch", "src": target})
    except ConnectionClosed as e:
        return {"error": f"bridge.unwatch_remote: send failed: {e}"}
    return {"ok": True, "unwatched": target}


# ─── factory: per-bundle verb table + dispatch ──────────────────


def make_verbs(
    *,
    build_transport: BuildTransport,
    sentence: str,
    reflect_fields: Callable[[dict, _BridgeState], dict],
    default_kind: str = "ws",
) -> dict:
    """Build the 6-verb table for a bridge bundle. `reflect_fields(rec, st)`
    supplies the transport-specific reflect fields; `sentence` is the one-liner;
    `default_kind` is the `transport` a record without one reflects. The leg's ingress/egress
    rules are resolved from the record by the shared registries (see boot)."""

    async def _boot(id, payload, kernel):
        """No args. Reads `transport` (kind) + transport-specific fields off the agent record, builds the transport, spawns the read loop, emits `bridge_up`. Idempotent: re-booting a connected bridge is a no-op."""
        return await boot(id, payload, kernel, build_transport=build_transport)

    async def _reconnect(id, payload, kernel):
        """No args. Teardown + boot — explicit because we don't auto-reconnect on transport failure (keeps real network problems visible to operators / telemetry)."""
        await _teardown(id)
        return await _boot(id, payload, kernel)

    async def _reflect(id, payload, kernel):
        """Identity + transport + connectivity. No args."""
        rec = kernel.get(id) or {}
        st = _state(id)
        out = {
            "id": id,
            "sentence": sentence,
            "transport": st.transport_kind or rec.get("transport") or default_kind,
            "connected": st.transport is not None and not st.transport.closed,
            "pending_count": len(st.pending),
            # The active per-leg rule TYPE names — keyed by the field you SET (config
            # like a `env` is never surfaced). `ingress_rule` (default
            # `deny_ingress`, the SEAL) + `egress_rule` (default `silent_egress`):
            # read-key == write-key, so `reflect` → edit is a direct mirror. IO legs
            # SEAL by default; SECURING is the default, OPENING is the conscious act.
            # The SAME `describe` every leg reflects with — the gate's own scoping,
            # so a bridge never reports a rule its gate resolves differently (an
            # `auth: allow_all_ingress` stem read as open while the gate denied).
            **describe(rec),
        }
        out.update(reflect_fields(rec, st))
        return out

    verbs = {
        "reflect": _reflect,
        "boot": _boot,
        "reconnect": _reconnect,
        "forward": _forward,
        "watch_remote": _watch_remote,
        "unwatch_remote": _unwatch_remote,
    }
    return verbs


#: What a bridge leg emits on its own inbox — served by `docs` (tier detailed);
#: each derivation re-exports it as its module's `EMITS`.
EMITS = {
    "bridge_up": "{type:'bridge_up'} — emitted on this agent's inbox after a successful boot",
    "bridge_down": "{type:'bridge_down'} — emitted when the transport drops (peer closed, tunnel died)",
    "<remote event>": "events from `watch_remote` subscriptions are re-emitted on this agent's inbox as their original `{type, ...}` shape plus `src` (the watched remote id); unsubscribed events are dropped",
}


async def dispatch(verbs: dict, id: str, payload: dict, kernel) -> dict | None:
    t = payload.get("type")
    fn = verbs.get(t)
    if fn is None:
        # Teach the envelope on a miss — the common slip is a frame with no
        # `type`, or a `forward` whose inner `payload` dropped its own `type`.
        return refusal(
            UNHANDLED,
            f"bridge: unknown type {t!r}",
            verb=t,
            hint=(
                'every bridge frame needs a "type"; to reach a remote agent use '
                '{"type":"forward","target":"<remote_id>",'
                '"payload":{"type":"<verb>",...}} — the inner payload is itself a '
                'verb frame and needs its own "type".'
            ),
        )
    return await fn(id, payload, kernel)
