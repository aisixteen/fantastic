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

Reply correlation: `corr_id = f"{bridge_id}:{counter}"` — namespacing eliminates
collisions across bridges; the frame `id` echoes it so logs correlate across hops.
"""

from __future__ import annotations

import asyncio
from dataclasses import dataclass, field
from typing import Any, Awaitable, Callable

from io_bridge._transport import ConnectionClosed, _BaseTransport
from io_bridge._base import Action, Decision, EgressRule, IngressRule, parse_spec
from io_bridge.channel import CredentialExtractor, EnvelopeExtractor
from io_bridge.ingress_rules import DenyInbound, resolve_ingress
from io_bridge.egress_rules import Silent, resolve_egress

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
# `st.cleanup` for transport-specific teardown (e.g. an ssh tunnel).
BuildTransport = Callable[[str, dict, Any, "_BridgeState"], Awaitable[_BaseTransport]]


def _rule_name(spec, default: str) -> str:
    """The rule TYPE name for reflect — never surfaces the rule's config (env var
    names, etc.). Absent spec ⇒ the caller's `default` (ingress: `deny_inbound`, the
    SEAL — IO legs seal by default; egress: `silent`)."""
    name, _ = parse_spec(spec)
    return name or default


def _ingress_name(rec: dict) -> str:
    spec = rec["ingress_rule"] if "ingress_rule" in rec else rec.get("auth")
    return _rule_name(spec, "deny_inbound")


def _egress_name(rec: dict) -> str:
    spec = rec["egress_rule"] if "egress_rule" in rec else rec.get("auth")
    return _rule_name(spec, "silent")


@dataclass
class _BridgeState:
    transport: _BaseTransport | None = None
    # Empty until `boot` succeeds — reflect falls through to the record's
    # `transport` field for the advertised config.
    transport_kind: str = ""
    read_task: asyncio.Task | None = None
    pending: dict[str, asyncio.Future] = field(default_factory=dict)
    corr_counter: int = 0
    # Optional transport-specific teardown the bundle registers in build_transport
    # (e.g. `lambda: _kill_tunnel(proc)` for ssh+ws). Engine calls it on teardown.
    cleanup: Callable[[], None] | None = None
    # Bundle-specific reflect/diagnostic state (e.g. ssh `tunnel_pid`). The engine
    # only clears it on teardown; the bundle's `reflect_fields` reads it.
    extra: dict = field(default_factory=dict)
    # Per-leg auth rules (symmetric, independent). The read loop consults `ingress`
    # before dispatching an inbound `call`; `forward` stamps `egress.credential()`
    # on the outbound envelope. Defaults: DenyInbound (sealed) inbound, Silent
    # outbound — but boot ALWAYS overwrites these from the record (resolve_ingress/egress).
    ingress: IngressRule = field(default_factory=DenyInbound)
    egress: EgressRule = field(default_factory=Silent)


# Per-agent bridge state — process-memory only, keyed by agent id (shared across
# all bridge bundles; ids are unique). Lost on restart, which is correct: a fresh
# kernel re-boots bridges via their `boot` verb.
_bridges: dict[str, _BridgeState] = {}

# Test seam: MemoryTransport tests inject a pre-built transport here before booting
# (a memory pair can't be carried in agent.json). `boot` pops it for `transport=memory`.
_test_transport_inject: dict[str, _BaseTransport] = {}


# ─── helpers ────────────────────────────────────────────────────


def _state(id: str) -> _BridgeState:
    s = _bridges.get(id)
    if s is None:
        s = _BridgeState()
        _bridges[id] = s
    return s


def _next_corr(id: str, st: _BridgeState) -> str:
    st.corr_counter += 1
    return f"{id}:{st.corr_counter}"


async def _read_loop(id: str, kernel: Any) -> None:
    """Long-lived consumer of the transport. Frame shapes:

      - inbound `call` — `{type:'call', id, target, payload}`: dispatch via
        `kernel.send(target, payload)`, reply `{type:'reply', id, data}`. (For a
        pure-client WS bridge this never fires — its peer is `web_ws`; for paired
        transports — MemoryTransport tests, the relay — it carries the callee role.)
      - inbound `reply` — resolve the pending Future for `id`.
      - inbound `error` — `{type:'error', id, error}` from a remote dispatch that
        RAISED; fail the pending forward promptly instead of hanging to timeout.
      - inbound `event` — `{type:'event', payload}` from a remote `watch`; re-emit
        on this bridge's inbox so local `kernel.watch(<bridge_id>, …)` sees it.

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
                target = frame.get("target")
                payload = frame.get("payload") or {}
                # AUTH GATE — the single shared choke point (`gate_inbound`, also used
                # by the web inbound legs). The leg's INGRESS rule decides whether the
                # peer may run this inbound call locally; a denial still gets a `reply`
                # (echoing the corr-id) so the caller resolves cleanly.
                decision = gate_inbound(st.ingress, "call", frame)
                if not decision.allowed:
                    # Teaching denial — mirror the web inbound legs so discovery-
                    # through-denial works over bridge legs too (reason + hint + see).
                    reply = {"error": decision.reason, "reason": "unauthorized"}
                    if decision.hint:
                        reply["hint"] = decision.hint
                    if decision.see:
                        reply["see"] = decision.see
                else:
                    try:
                        reply = await kernel.send(target, payload)
                    except Exception as e:
                        reply = {"error": f"bridge: kernel.send raised: {e}"}
                await transport.send(
                    {"type": "reply", "id": frame.get("id"), "data": reply}
                )
            elif ftype == "reply":
                fut = st.pending.pop(frame.get("id"), None)
                if fut is not None and not fut.done():
                    fut.set_result(frame.get("data"))
            elif ftype == "error":
                fut = st.pending.pop(frame.get("id"), None)
                if fut is not None and not fut.done():
                    fut.set_result({"error": f"remote error: {frame.get('error')}"})
            elif ftype == "event":
                # The `watch_remote` RETURN half: WE dialed OUT to a chosen, key-pinned
                # peer and asked it to stream; this re-emit lands on the bridge's OWN
                # inbox for explicit local watchers — trusted-by-association (we opened
                # the channel), NOT a sealed-ingress inbound leak.
                try:
                    await kernel.emit(id, frame.get("payload") or {})
                except Exception:
                    pass
            # Other frame types are ignored so the loop stays robust.
    finally:
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
    if st.transport is not None:
        try:
            await st.transport.close()
        except Exception:
            pass
    if st.cleanup is not None:
        try:
            st.cleanup()
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
    st.ingress = DenyInbound()
    st.egress = Silent()


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
    seam for every other kind, then the common tail (resolve the leg's ingress +
    egress rules, spawn read loop, emit `bridge_up`)."""
    rec = kernel.get(id) or {}
    st = _state(id)
    if st.transport is not None and not st.transport.closed:
        return {"already": True, "transport": st.transport_kind}

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
                    st.cleanup()
                except Exception:
                    pass
                st.cleanup = None
            return {"error": f"bridge: {kind} boot failed: {e}"}

    st.transport = transport
    st.transport_kind = kind
    try:
        st.ingress = resolve_ingress(rec)
        st.egress = resolve_egress(rec)
    except Exception as e:
        # A bad rule must not leave the just-built transport leaked/open.
        await _teardown(id)
        return {"error": f"bridge: bad auth rule: {e}"}
    st.read_task = asyncio.create_task(_read_loop(id, kernel))
    await kernel.emit(id, {"type": "bridge_up"})
    return {"booted": True, "transport": kind}


# ─── shared verbs (transport-agnostic) ──────────────────────────


async def _forward(id, payload, kernel):
    """args: target:str (req — id on the REMOTE kernel) + payload:dict (req — the call to run there; payload is ITSELF a verb frame and MUST carry its own "type") + timeout:float? (default 30s). Example: {"type":"forward","target":"src","payload":{"type":"read_stream","path":"data.bin","offset":0}}. Ships a raw `{type:'call', id, target, payload}` frame over the transport, awaits the matching reply, returns the unwrapped data. Local→local stays direct kernel.send (this verb is only for cross-kernel). Multi-hop loop detection is the caller's responsibility — the bridge ships whatever it's given."""
    target = payload.get("target")
    inner = payload.get("payload")
    if not target or not isinstance(inner, dict):
        return {"error": "bridge.forward: target (str) + payload (dict) required"}
    timeout = float(payload.get("timeout", DEFAULT_FORWARD_TIMEOUT))

    st = _state(id)
    if st.transport is None or st.transport.closed:
        return {"error": "bridge.forward: not connected (call boot first)"}

    corr = _next_corr(id, st)
    fut: asyncio.Future = asyncio.get_event_loop().create_future()
    st.pending[corr] = fut

    frame = {"type": "call", "id": corr, "target": target, "payload": inner}
    # Stamp this leg's EGRESS credential on the envelope via the shared decorator.
    frame = stamp_egress(st.egress, frame)
    try:
        await st.transport.send(frame)
    except ConnectionClosed as e:
        st.pending.pop(corr, None)
        return {"error": f"bridge.forward: send failed: {e}"}

    try:
        return await asyncio.wait_for(fut, timeout=timeout)
    except asyncio.TimeoutError:
        st.pending.pop(corr, None)
        return {"error": f"bridge.forward: timeout after {timeout}s"}
    except ConnectionError as e:
        return {"error": f"bridge.forward: {e}"}


async def _watch_remote(id, payload, kernel):
    """args: target:str (req — id on the REMOTE kernel to watch). Sends `{type:'watch', src:<target>}` over the transport. Subsequent `{type:'event'}` frames from the remote arrive via the read loop and are re-emitted on THIS bridge agent's inbox. Local watchers subscribe to the bridge with `kernel.watch(<bridge_id>, ...)` and see the remote stream. Idempotent on the wire (web_ws de-dups via its own `watching` set)."""
    target = payload.get("target")
    if not target or not isinstance(target, str):
        return {"error": "bridge.watch_remote: target (str) required"}
    st = _state(id)
    if st.transport is None or st.transport.closed:
        return {"error": "bridge.watch_remote: not connected (call boot first)"}
    try:
        await st.transport.send({"type": "watch", "src": target})
    except ConnectionClosed as e:
        return {"error": f"bridge.watch_remote: send failed: {e}"}
    return {"ok": True, "watching": target}


async def _unwatch_remote(id, payload, kernel):
    """args: target:str (req — id previously passed to watch_remote). Sends `{type:'unwatch', src:<target>}` to the remote so it stops emitting events for this subscription. Events already in-flight on the wire are still delivered + re-emitted."""
    target = payload.get("target")
    if not target or not isinstance(target, str):
        return {"error": "bridge.unwatch_remote: target (str) required"}
    st = _state(id)
    if st.transport is None or st.transport.closed:
        return {"error": "bridge.unwatch_remote: not connected (call boot first)"}
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
    `default_kind` is the reflect `transport` fallback. The leg's ingress/egress
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
            # like a `token_env` is never surfaced). `ingress_rule` (default
            # `deny_inbound`, the SEAL) + `egress_rule` (default `silent`): read-key ==
            # write-key, so `reflect` → edit is a direct mirror. IO legs SEAL by default;
            # SECURING is the default, OPENING is the conscious act.
            "ingress_rule": _ingress_name(rec),
            "egress_rule": _egress_name(rec),
            "sealed": _ingress_name(rec) != "allow_all",
        }
        out.update(reflect_fields(rec, st))
        out["verbs"] = {
            n: (f.__doc__ or "").strip().splitlines()[0] for n, f in verbs.items()
        }
        out["emits"] = {
            "bridge_up": "{type:'bridge_up'} — emitted on this agent's inbox after a successful boot",
            "bridge_down": "{type:'bridge_down'} — emitted when the transport drops (peer closed, tunnel died)",
            "<remote event>": "events from `watch_remote` subscriptions are re-emitted on this agent's inbox with their original `{type, ...}` shape",
        }
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


async def dispatch(verbs: dict, id: str, payload: dict, kernel) -> dict | None:
    t = payload.get("type")
    fn = verbs.get(t)
    if fn is None:
        # Teach the envelope on a miss — the common slip is a frame with no
        # `type`, or a `forward` whose inner `payload` dropped its own `type`.
        return {
            "error": f"bridge: unknown type {t!r}",
            "hint": (
                'every bridge frame needs a "type"; to reach a remote agent use '
                '{"type":"forward","target":"<remote_id>",'
                '"payload":{"type":"<verb>",...}} — the inner payload is itself a '
                'verb frame and needs its own "type".'
            ),
        }
    return await fn(id, payload, kernel)
