"""Reply routing on the AI agent — the model decides where its output goes.

After ONE provider pass the output is routed. Two paths, and the FIRST is the model's:

  its own `<call>`  — names a target, a verb and `reply_to`. The call is
    dispatched (exactly one per pass, never a loop) and its REPLY is delivered to
    `reply_to`. `reply_to:'self'` is what makes a chain, one step at a time.
    NO CALL ENDS THE TURN — stop is the default, so a one-pass task costs one pass.

  the caller's `send_to`  — hands this pass's TEXT to another agent, once, without
    propagating.

The chain is DETACHED, which is the whole reason it works: the per-backend FIFO lock is
a plain non-reentrant asyncio.Lock, so a self-addressed send awaited inline would block
on a lock its own caller holds. `test_self_chain_does_not_deadlock` is that regression.
"""

from __future__ import annotations

from _testkit import ensure_memory
import asyncio

import ollama_backend.tools as ot
from ai_core.tool_parse import extract_tool_calls, render_call


def _call(target: str, payload: dict, reply_to: str | None = "self") -> str:
    """The envelope exactly as the prompt teaches it."""
    args: dict = {"target_id": target, "payload": payload}
    if reply_to is not None:
        args["reply_to"] = reply_to
    return render_call("send", args)


class _FakeProvider:
    """Yields a scripted stream per `chat()` call, and records what it was asked."""

    def __init__(self, scripts: list[list]):
        self._scripts = list(scripts)
        self.calls = 0
        self.requests: list = []

    async def chat(self, messages, max_tokens=None):
        self.requests.append(messages)
        items = self._scripts.pop(0) if self._scripts else []
        self.calls += 1
        for x in items:
            yield x


async def _make_ollama(kernel, grants=None):
    # `context_window` explicitly: the default (4096) is smaller than the system block
    # once the envelope howto is in it, so a second turn would die on
    # `context_insufficient` rather than on anything these tests are about.
    #
    # Deny-all: the backend streams its turn to the `cli` renderer via send (the
    # default client) → grant that listener; `grants` adds any PEER the model in a
    # given test routes to (a `send`/`send_to` to another agent is lateral egress).
    store = ensure_memory(kernel)
    can_call = {"cli": "*", store: "*", **(grants or {})}
    rec = (
        await kernel.send(
            [
                {
                    "target": "kernel_state",
                    "payload": {
                        "type": "create_agent",
                        "parent_id": "kernel_state",
                        "handler_module": "ollama_backend.tools",
                        # required at the write (the schema); the provider is faked, never dialled
                        "endpoint": "http://127.0.0.1:9/stub",
                        "model": "stub",
                        "context_window": 32768,
                        "can_call": can_call,
                        "memory_store_id": store,
                    },
                }
            ]
        )
    )[0]["result"]
    aid = rec["id"]
    # Two-sided deny-all: every granted egress needs the callee's reciprocal
    # `callable_by` (the backend streams its turn to `cli` and routes to peers).
    for target in can_call:
        await _accept(kernel, target, aid)
    return aid


async def _accept(kernel, target: str, caller: str, verbs="*") -> None:
    """Open the callee's ingress so a granted egress actually lands — merges,
    keeping the callee's structural baseline `callable_by`."""
    tgt = kernel.ctx.agents.get(target)
    cb = dict(tgt._meta.get("callable_by") or {}) if tgt is not None else {}
    cb[caller] = verbs
    (
        await kernel.send(
            [
                {
                    "target": "kernel_state",
                    "payload": {
                        "type": "update_agent",
                        "id": target,
                        "callable_by": cb,
                    },
                }
            ]
        )
    )[0]["result"]


async def _until_calls(fp, n: int, timeout: float = 5.0) -> bool:
    end = asyncio.get_event_loop().time() + timeout
    while asyncio.get_event_loop().time() < end:
        if fp.calls >= n:
            return True
        await asyncio.sleep(0.02)
    return False


# ─── stop is the default ────────────────────────────────────────


async def test_done_means_one_pass(seeded_kernel, store_agent):
    """THE point of model-owned routing: a one-pass task costs ONE pass. A turn
    that answers and calls `done` is finished — nothing schedules another turn.
    (A turn with NO call is nudged: all is tool calls, 2026-09-09.)"""
    oid = await _make_ollama(seeded_kernel)
    fp = _FakeProvider([['the answer <call name="done"></call>'], ["must never run"]])
    ot._providers[oid] = fp
    try:
        r = (
            await seeded_kernel.send(
                [{"target": oid, "payload": {"type": "send", "text": "hi"}}]
            )
        )[0]["result"]
        assert "the answer" in r["final"]
        await asyncio.sleep(0.3)
        assert fp.calls == 1, f"done must end the turn, got {fp.calls} passes"
    finally:
        ot._providers.pop(oid, None)


# ─── the model acts, and sees the result ────────────────────────


async def test_model_call_is_dispatched_and_the_result_returns(
    seeded_kernel, store_agent
):
    """The model emits a send; it really runs; its REPLY comes back as the next turn,
    tagged so the model can tell it from a human's message.

    The follow-up ends the chain EXPLICITLY with the `done` call — so this is
    TWO passes: the call, then its done-signalled answer."""
    oid = await _make_ollama(seeded_kernel)
    fp = _FakeProvider(
        [
            [_call("kernel_state", {"type": "list_agents"})],
            ['done looking <call name="done"></call>'],
        ]
    )
    ot._providers[oid] = fp
    try:
        (
            await seeded_kernel.send(
                [{"target": oid, "payload": {"type": "send", "text": "what is here?"}}]
            )
        )[0]["result"]
        assert await _until_calls(fp, 2), "the call's reply never came back"
        await asyncio.sleep(0.2)
        assert fp.calls == 2, "the done-signalled follow-up ended the chain"
        turn2 = str(fp.requests[1])
        assert "· kernel_state list_agents · ↑" in turn2, (
            f"untagged reply: {turn2[-400:]}"
        )
        assert "agents" in turn2, "the real list_agents result must be in the turn"
    finally:
        ot._providers.pop(oid, None)


async def test_self_chain_does_not_deadlock(seeded_kernel, store_agent):
    """THE regression. A self-addressed reply must QUEUE behind the current turn, not
    deadlock on the FIFO lock it already holds — that would stall until `send_timeout`
    (180s), so this test failing fast IS the signal."""
    oid = await _make_ollama(seeded_kernel)
    fp = _FakeProvider(
        [
            [_call("kernel_state", {"type": "list_agents"})],
            [_call("kernel_state", {"type": "list_agents"})],
            # the chain ends EXPLICITLY — prose alone would draw the nudge now
            ['enough <call name="done"></call>'],
        ]
    )
    ot._providers[oid] = fp
    try:
        (
            await seeded_kernel.send(
                [{"target": oid, "payload": {"type": "send", "text": "go"}}]
            )
        )[0]["result"]
        assert await _until_calls(fp, 3), f"chain stalled at {fp.calls} passes"
        await asyncio.sleep(0.3)
        assert fp.calls == 3, f"expected 3 passes, got {fp.calls}"
    finally:
        ot._providers.pop(oid, None)


async def test_reply_to_omitted_defaults_to_self(seeded_kernel, store_agent):
    """AN OMITTED `reply_to` MEANS SELF — the model gets its own answer back.

    The old default was fire-and-forget, and it produced the one failure a model
    cannot debug: gemma4:12b left the key out of an otherwise perfect call, the
    send ran, its result was dropped, and the chain died at pass 1 with no reply
    and no error. Indistinguishable, from inside the model, from being finished.
    Stopping is explicit — the `done` call ends a chain."""
    other = await _make_ollama(seeded_kernel)
    oid = await _make_ollama(seeded_kernel, grants={other: ["send"]})
    fo = _FakeProvider([["peer answered"]])
    fp = _FakeProvider([[_call(other, {"type": "send", "text": "ping"}, None)], ["x"]])
    ot._providers[oid] = fp
    ot._providers[other] = fo
    try:
        (
            await seeded_kernel.send(
                [{"target": oid, "payload": {"type": "send", "text": "go"}}]
            )
        )[0]["result"]
        assert await _until_calls(fo, 1), "the send itself must still happen"
        assert await _until_calls(fp, 2), "an omitted reply_to must come back to self"
        assert "[reply to " in str(fp.requests[1]), "and arrive tagged as a reply"
    finally:
        ot._providers.pop(oid, None)
        ot._providers.pop(other, None)


async def test_discarding_a_reply_must_be_said_explicitly(seeded_kernel, store_agent):
    """Fire-and-forget still exists — it just has to be ASKED for. `reply_to:null`
    (or "none") acts and ends the chain; that is the right way round, because
    throwing an answer away is the surprising choice, not the default one."""
    other = await _make_ollama(seeded_kernel)
    oid = await _make_ollama(seeded_kernel, grants={other: ["send"]})
    fo = _FakeProvider([["peer answered"]])
    fp = _FakeProvider(
        [[_call(other, {"type": "send", "text": "ping"}, "null")], ["x"]]
    )
    ot._providers[oid] = fp
    ot._providers[other] = fo
    try:
        (
            await seeded_kernel.send(
                [{"target": oid, "payload": {"type": "send", "text": "go"}}]
            )
        )[0]["result"]
        assert await _until_calls(fo, 1), "the send itself must still happen"
        await asyncio.sleep(0.3)
        assert fp.calls == 1, "an explicit null must mean no follow-up turn"
    finally:
        ot._providers.pop(oid, None)
        ot._providers.pop(other, None)


async def test_failed_call_comes_back_to_the_model(seeded_kernel, store_agent):
    """A call that fails must be VISIBLE to the model — a silent failure is the one
    thing it can never debug."""
    oid = await _make_ollama(seeded_kernel)
    fp = _FakeProvider([[_call("no_such_agent", {"type": "reflect"})], ["ok"]])
    ot._providers[oid] = fp
    try:
        (
            await seeded_kernel.send(
                [{"target": oid, "payload": {"type": "send", "text": "go"}}]
            )
        )[0]["result"]
        assert await _until_calls(fp, 2), "the failure never reached the model"
        turn2 = str(fp.requests[1]).lower()
        assert "no_such_agent" in turn2
        assert "error" in turn2 or "unknown" in turn2, f"not an error: {turn2[-300:]}"
    finally:
        ot._providers.pop(oid, None)


async def test_malformed_call_is_reported_not_swallowed(seeded_kernel, store_agent):
    """A `<call>` with no target tells the model so, instead of vanishing."""
    oid = await _make_ollama(seeded_kernel)
    bad = '<call name="send"><reply_to>self</reply_to><type>read</type></call>'
    fp = _FakeProvider([[bad], ["ok"]])
    ot._providers[oid] = fp
    try:
        (
            await seeded_kernel.send(
                [{"target": oid, "payload": {"type": "send", "text": "go"}}]
            )
        )[0]["result"]
        assert await _until_calls(fp, 2), "malformed call was swallowed"
        assert "a send names its target" in str(fp.requests[1])
    finally:
        ot._providers.pop(oid, None)


async def test_a_send_with_no_payload_names_what_is_missing(seeded_kernel, store_agent):
    """Measured 2026-08-06: a payload-less send reached the handler as `None` and
    came back `'NoneType' object has no attribute 'get'` — a Python error about our
    internals, naming neither the omitted verb nor the agent. It must say what was
    missing instead, and must never dispatch."""
    oid = await _make_ollama(seeded_kernel)
    bad = '<call name="send"><target_id>totals</target_id></call>'
    fp = _FakeProvider([[bad], ["ok"]])
    ot._providers[oid] = fp
    try:
        (
            await seeded_kernel.send(
                [{"target": oid, "payload": {"type": "send", "text": "go"}}]
            )
        )[0]["result"]
        assert await _until_calls(fp, 2), "the payload-less send was swallowed"
        turn2 = str(fp.requests[1])
        assert "no payload" in turn2, turn2[-300:]
        assert "NoneType" not in turn2, "must not surface an internal Python error"
    finally:
        ot._providers.pop(oid, None)


async def test_an_unparseable_call_reaches_the_model_as_an_error(
    seeded_kernel, store_agent
):
    """END TO END, which the parser-level tests do not cover — and the gap that let
    this ship broken: on 2026-08-06 `malformed_calls` returned the right report for
    gemma's truncated `create_agent` and the model still received nothing, so the
    run went on believing its agent existed."""
    oid = await _make_ollama(seeded_kernel)
    broken = (
        '<call name="send"><target_id>kernel</target_id><type>create_agent</type>'
        '<bytes>x = "unclosed</call>'
    )
    fp = _FakeProvider([[broken], ["ok"]])
    ot._providers[oid] = fp
    try:
        (
            await seeded_kernel.send(
                [{"target": oid, "payload": {"type": "send", "text": "go"}}]
            )
        )[0]["result"]
        assert await _until_calls(fp, 2), "the malformed call produced no next turn"
        turn2 = str(fp.requests[1])
        assert "NOT dispatched" in turn2, turn2[-400:]
    finally:
        ot._providers.pop(oid, None)


async def test_a_verb_named_tool_reaches_the_agent(seeded_kernel, store_agent):
    """`<call name="reflect">` compresses the verb into the tool name. Under the one
    grammar that is REFUSED with the shape (a verb is `<type>`), never guessed."""
    oid = await _make_ollama(seeded_kernel)
    call = '<call name="reflect"><target_id>totals</target_id></call>'
    fp = _FakeProvider([[call], ["ok"]])
    ot._providers[oid] = fp
    try:
        (
            await seeded_kernel.send(
                [{"target": oid, "payload": {"type": "send", "text": "go"}}]
            )
        )[0]["result"]
        assert await _until_calls(fp, 2), "the reflect never came back"
        turn2 = str(fp.requests[1])
        assert "is not a tool" in turn2 and "<type>reflect</type>" in turn2, turn2[
            -400:
        ]
    finally:
        ot._providers.pop(oid, None)


# ─── the caller's one-shot handoff ──────────────────────────────


async def test_caller_send_to_hands_off_once(seeded_kernel, store_agent):
    """`send_to` delivers this pass's TEXT to a peer — once. It must NOT propagate:
    a caller cannot spend the model's turns for it."""
    b = await _make_ollama(seeded_kernel)
    a = await _make_ollama(seeded_kernel, grants={b: ["send"]})
    fa = _FakeProvider(
        [['the answer from a <call name="done"></call>'], ["a must not run again"]]
    )
    fb = _FakeProvider(
        [['b replied <call name="done"></call>'], ["b must not run again"]]
    )
    ot._providers[a] = fa
    ot._providers[b] = fb
    try:
        (
            await seeded_kernel.send(
                [
                    {
                        "target": a,
                        "payload": {
                            "type": "send",
                            "text": "go",
                            "client_id": "probe",
                            "send_to": b,
                        },
                    }
                ]
            )
        )[0]["result"]
        assert await _until_calls(fb, 1), "the peer never received the reply"
        await asyncio.sleep(0.3)
        assert "the answer from a" in str(fb.requests[0])
        assert fa.calls == 1 and fb.calls == 1, "send_to must not propagate"
    finally:
        ot._providers.pop(a, None)
        ot._providers.pop(b, None)


async def test_error_reply_does_not_route(seeded_kernel, store_agent):
    """A failed pass is terminal — a broken chain must not keep spending turns."""
    oid = await _make_ollama(seeded_kernel)

    class _Boom:
        calls = 0

        async def chat(self, messages, max_tokens=None):
            _Boom.calls += 1
            raise RuntimeError("provider exploded")
            yield  # pragma: no cover - generator marker

    ot._providers[oid] = _Boom()
    try:
        try:
            (
                await seeded_kernel.send(
                    [
                        {
                            "target": oid,
                            "payload": {
                                "type": "send",
                                "text": "go",
                                "client_id": "probe",
                            },
                        }
                    ]
                )
            )[0]["result"]
        except RuntimeError:
            pass
        await asyncio.sleep(0.3)
        assert _Boom.calls == 1
    finally:
        ot._providers.pop(oid, None)


# ─── the emergence discipline, guarded ──────────────────────────


async def test_prompt_states_the_mechanism_not_a_strategy(seeded_kernel, store_agent):
    """The default system block may state FACTS — how to reach an agent, what reflect
    returns, where `reply_to` sends the answer. It must NOT name the STRATEGY.

    The removed `_SEND_HOWTO` (recoverable at 2ce23d1) did exactly that: "either call
    again or write your final answer to finish", "ORIENT FIRST", "NEVER claim you don't
    have access". That is the loop, dictated — reusing that wording because it is
    'tuned' would quietly void every emergence result taken afterwards.
    """
    oid = await _make_ollama(seeded_kernel)
    # `_assemble` returns (messages, store_len) — the store length is what the
    # compaction mark records, so it cannot be derived from the model view.
    msgs, _store_len = await ot._assemble(oid, "hi", seeded_kernel, "probe")
    sys_block = msgs[0]["content"].lower()

    # the mechanism IS stated
    assert "reply_to" in sys_block
    assert "reflect" in sys_block
    assert "<call name=" in sys_block

    banned = [
        "loop",
        "iterate",
        "iteration",
        "step by step",
        "keep going",
        "until done",
        "until you are done",
        "agentic",
        "continue working",
        "call again",
        "final answer",
        "to finish",
        "orient first",
    ]
    leaked = [w for w in banned if w in sys_block]
    assert not leaked, f"system prompt leaks strategy {leaked} — voids the test"


async def test_a_raising_handler_comes_back_as_an_error_not_a_dead_chain(
    seeded_kernel, store_agent
):
    """A handler that RAISES must reach the model as `{"error": ...}`.

    This ran bare until 2026-08-02: a file_bridge whose ingress rule could not be
    resolved raised out of its handler, the exception escaped `_route_reply`, and
    asyncio logged "Task exception was never retrieved". The model got no reply, no
    error and no further turn — the run stopped mid-verification and looked
    finished. An exception is a result; the model can act on one and cannot act on
    silence."""
    import sys
    import types

    async def _handler(_id, payload, _kernel):
        if payload.get("type") in ("boot", "shutdown"):
            return None
        raise ValueError("unknown ingress rule type 'password:whatever'")

    mod = types.ModuleType("raiser.tools")
    mod.handler = _handler
    mod.VERBS = {}
    sys.modules["raiser.tools"] = mod

    rec = (
        await seeded_kernel.send(
            [
                {
                    "target": "kernel",
                    "payload": {
                        "type": "create_agent",
                        "parent_id": "kernel_state",
                        "handler_module": "raiser.tools",
                        "id": "raiser",
                    },
                }
            ]
        )
    )[0]["result"]
    # oid needs egress to raiser (and _make_ollama opens raiser's reciprocal ingress).
    oid = await _make_ollama(seeded_kernel, grants={"raiser": ["reflect"]})
    fp = _FakeProvider([[_call(rec["id"], {"type": "reflect"})], ["saw it"]])
    ot._providers[oid] = fp
    try:
        (
            await seeded_kernel.send(
                [{"target": oid, "payload": {"type": "send", "text": "go"}}]
            )
        )[0]["result"]
        assert await _until_calls(fp, 2), (
            "the raise killed the chain instead of routing"
        )
        turn2 = str(fp.requests[1])
        assert "ValueError" in turn2, f"the exception must be visible: {turn2[-300:]}"
        assert "password:whatever" in turn2, "and must carry what actually went wrong"
    finally:
        ot._providers.pop(oid, None)


async def test_thinking_is_parsed_but_not_persisted(seeded_kernel, store_agent):
    """A tool call written mid-thought DISPATCHES; the deliberation is NOT kept.

    Both halves matter and they pull opposite ways. Read only `content` and a turn
    spent reasoning is an empty completion that silently ends the chain (gemma4:12b,
    2026-08-02). Keep the reasoning and it is re-sent in every later prompt —
    gemma's 3,800-char turn-1 deliberation crowding out the task — which is also
    against every vendor's guidance for reasoning models. So: parse the full text,
    persist the stripped text."""
    from ai_core.core import THINK_CLOSE, THINK_OPEN, without_thinking

    raw = (
        f"{THINK_OPEN}Let me work out which agent stores data. Probably notes."
        f"{THINK_CLOSE}" + _call("kernel_state", {"type": "list_agents"}) + "on it"
    )
    # PARSE sees through the thinking…
    assert extract_tool_calls(raw), "the call must still be found"
    # …and PERSIST does not keep it.
    kept = without_thinking(raw)
    assert "Let me work out" not in kept
    assert "<call " in kept and "on it" in kept
    # an unterminated span (stream cut mid-thought) drops its tail rather than leaking
    assert without_thinking(f"visible{THINK_OPEN}secret") == "visible"

    oid = await _make_ollama(seeded_kernel)
    fp = _FakeProvider([[raw], ["done"]])
    ot._providers[oid] = fp
    try:
        (
            await seeded_kernel.send(
                [{"target": oid, "payload": {"type": "send", "text": "go"}}]
            )
        )[0]["result"]
        assert await _until_calls(fp, 2), "a call inside thinking must still dispatch"
        assert "· kernel_state list_agents · ↑" in str(fp.requests[1])
    finally:
        ot._providers.pop(oid, None)
