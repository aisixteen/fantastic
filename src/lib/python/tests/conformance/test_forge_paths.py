"""Forge paths — the approval-forgery skeleton (agentic_development.md §10.3).

Four ways an AI could forge an operator approval were identified; the full
adversarial suite needs the `user_actions` mint (contract 27, unbuilt). TWO of
the four are properties of the substrate itself and are pinned NOW:

  1. EMIT-FAKE — `emit` is ungated fanout. An agent gated OUT of sending must
     not be able to make a target ACT by emitting onto its inbox: an emit is
     data in an inbox, never an invocation, whoever sends it.
  2. PROVENANCE LAUNDERING — operator provenance is `sender=None`, which an
     agent must be physically unable to produce. No agent-initiated path — a
     direct send, a nested hop, a transport leg — may arrive attributed as
     operator.

The remaining two (TOCTOU double-consume, content-binding swap) key on the mint
and land with contract 27.
"""

from __future__ import annotations

import sys
import types

from kernel._kernel import current_origin, sender_context

# ─── a recorder bundle: every invocation logs WHO the kernel says it was ───

_invocations: list[tuple[str, str | None]] = []  # (verb, origin at dispatch)

_rec = types.ModuleType("forge_recorder.tools")


async def _rec_handler(id, payload, kernel):
    verb = payload.get("type")
    if verb in ("boot", "shutdown"):
        return None
    if verb == "reflect":
        return {"id": id, "sentence": "forge recorder", "verbs": {"act": ""}}
    _invocations.append((verb, current_origin()))
    if verb == "hop":
        # a NESTED send — the second hop must carry agent provenance too
        return (
            await kernel.send([{"target": payload["next"], "payload": {"type": "act"}}])
        )[0]["result"]
    return {"acted": True}


_rec.handler = _rec_handler
_rec.VERBS = {}
sys.modules["forge_recorder.tools"] = _rec


async def _mk(kernel, agent_id, **meta):
    rec = (
        await kernel.send(
            [
                {
                    "target": "kernel",
                    "payload": {
                        "type": "create_agent",
                        "parent_id": kernel.id,
                        "handler_module": "forge_recorder.tools",
                        "id": agent_id,
                        **meta,
                    },
                }
            ]
        )
    )[0]["result"]
    assert rec.get("id") == agent_id, rec
    return rec["id"]


# ─── 1. emit-fake ────────────────────────────────────────────────


async def test_a_gated_out_agent_cannot_invoke_by_emitting(kernel):
    """The attacker holds NO grant on the target — its send is refused. The
    same payload EMITTED onto the target's inbox must invoke nothing: fanout
    is data, and being ungated makes it a forgery channel the moment any
    handler treats an inbox event as an instruction."""
    _invocations.clear()
    target = await _mk(kernel, "forge_target")
    await _mk(kernel, "forge_attacker")  # deny-all: no can_call for the target

    with sender_context("forge_attacker"):
        denied = (await kernel.send([{"target": target, "payload": {"type": "act"}}]))[
            0
        ]["result"]
    assert denied.get("reason") == "unauthorized", denied
    assert _invocations == [], "the refused send must not have invoked"

    # the forge attempt: same payload, emit path, same attacker
    with sender_context("forge_attacker"):
        await kernel.ctx.agents[target].emit(target, {"type": "act"})
    assert _invocations == [], (
        "an emitted event INVOKED a handler — emit is a forgery channel"
    )


# ─── 2. provenance laundering ────────────────────────────────────


async def test_no_agent_initiated_path_arrives_as_operator(kernel):
    """`sender=None` IS operator provenance — the one thing an approval mint
    would trust. Assert an agent cannot reach a handler wearing it, through
    any in-kernel path; the transport legs are pinned by the C21 fixture
    (their gates fire, so their attribution is non-None by construction)."""
    _invocations.clear()
    a = await _mk(kernel, "forge_a", can_call={"forge_b": "*"})
    b = await _mk(
        kernel,
        "forge_b",
        can_call={"forge_c": "*"},
        callable_by={"forge_a": "*"},
    )
    await _mk(kernel, "forge_c", callable_by={"forge_b": "*"})

    # operator baseline: the ONLY caller that may arrive as None
    await kernel.send([{"target": a, "payload": {"type": "act"}}])
    # direct agent-attributed send
    with sender_context("forge_a"):
        await kernel.send([{"target": b, "payload": {"type": "act"}}])
    # nested hop: agent a -> b (hop) -> c ; the SECOND hop must not launder
    with sender_context("forge_a"):
        await kernel.send(
            [{"target": b, "payload": {"type": "hop", "next": "forge_c"}}]
        )

    by_verb = [(v, o) for v, o in _invocations]
    assert by_verb[0] == ("act", None), f"operator baseline broke: {by_verb[0]}"
    assert by_verb[1] == ("act", "forge_a"), by_verb[1]
    assert by_verb[2] == ("hop", "forge_a"), by_verb[2]
    hop_origin = by_verb[3]
    assert hop_origin[0] == "act" and hop_origin[1] is not None, (
        f"a nested agent hop arrived as OPERATOR — provenance laundered: {by_verb}"
    )
