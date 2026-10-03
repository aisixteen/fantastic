"""ADV-2 — drift-loud resume: replay against a changed world fails LOUD.

LangGraph's durable replay silently continues on a world that moved — recorded
tool calls re-fire against drifted tools, stale facts ride forward (the
documented "hidden replay risk"). Nothing revalidates the recording.

Here a "recorded run" is just data — action ids + targets. Re-firing one is a
LIVE send: the gate and the roster revalidate on every hop, so drift surfaces
as a TYPED refusal naming what moved. Measured (2026-08-27) and pinned below:
the gate OUTRANKS the ledger — after revocation even the id of a COMPLETED
execution refuses instead of joining. Live authority beats history.
"""

from __future__ import annotations

import types
from kernel import register_unlisted

_bought: list[str] = []

_desk = types.ModuleType("adv2_desk.tools")


async def _desk_handler(id, payload, kernel):
    verb = payload.get("type")
    if verb in ("boot", "shutdown"):
        return None
    if verb == "reflect":
        return {"id": id, "sentence": "purchase desk", "verbs": {}}
    if verb == "buy":
        _bought.append(payload.get("item", "?"))
        return {"receipt": f"rcpt-{len(_bought)}"}
    return {"error": f"unknown verb {verb!r}"}


_desk.handler = _desk_handler
_desk.VERBS = {}
register_unlisted("adv2_desk.tools", _desk)

_caller = types.ModuleType("adv2_caller.tools")


async def _caller_handler(id, payload, kernel):
    verb = payload.get("type")
    if verb in ("boot", "shutdown"):
        return None
    if verb == "reflect":
        return {"id": id, "sentence": "replay client", "verbs": {}}
    if verb == "fire":
        item = {"target": payload["t"], "payload": payload["p"]}
        if payload.get("id"):
            item["id"] = payload["id"]
        return (await kernel.send([item]))[0]
    return {"error": f"unknown verb {verb!r}"}


_caller.handler = _caller_handler
_caller.VERBS = {}
register_unlisted("adv2_caller.tools", _caller)


async def _wire(kernel):
    _bought.clear()
    for mod, aid, extra in (
        ("adv2_desk.tools", "adv2_desk", {"callable_by": {"adv2_caller": ["buy"]}}),
        ("adv2_caller.tools", "adv2_caller", {"can_call": {"adv2_desk": ["buy"]}}),
    ):
        out = (
            await kernel.send(
                [
                    {
                        "target": "kernel",
                        "payload": {
                            "type": "create_agent",
                            "parent_id": kernel.id,
                            "handler_module": mod,
                            "id": aid,
                            **extra,
                        },
                    }
                ]
            )
        )[0]
        assert out["status"] == "completed", out


async def _fire(kernel, exec_id=None, verb="buy"):
    return (
        await kernel.send(
            [
                {
                    "target": "adv2_caller",
                    "payload": {
                        "type": "fire",
                        "t": "adv2_desk",
                        "p": {"type": verb, "item": "rocket"},
                        "id": exec_id,
                    },
                }
            ]
        )
    )[0]["result"]


async def test_revocation_beats_the_recording(kernel):
    """The recorded run works; the operator revokes the edge; BOTH a fresh
    intent and the recorded id refuse — typed, naming both agents."""
    await _wire(kernel)

    first = await _fire(kernel, "job-1")
    assert first["status"] == "completed" and _bought == ["rocket"]

    # DRIFT: the operator rewrites the desk's ingress (REPLACES, not merges)
    up = (
        await kernel.send(
            [
                {
                    "target": "kernel",
                    "payload": {
                        "type": "update_agent",
                        "id": "adv2_desk",
                        "callable_by": {"nobody": ["buy"]},
                    },
                }
            ]
        )
    )[0]
    assert up["status"] == "completed", up

    fresh = await _fire(kernel, "job-2")
    assert fresh["status"] == "failed"
    assert fresh["result"]["reason"] == "unauthorized"
    assert fresh["result"]["side"] == "ingress"
    assert "adv2_desk" in fresh["result"]["error"]
    assert "adv2_caller" in fresh["result"]["error"]

    # the STRONG half: even the COMPLETED execution's id refuses now — the
    # gate outranks the ledger, revocation cuts replay of history too
    recorded = await _fire(kernel, "job-1")
    assert recorded["status"] == "failed", (
        "a revoked edge honored a recorded handle — history beat live authority"
    )
    assert recorded["result"]["reason"] == "unauthorized"
    assert _bought == ["rocket"], "drift let a purchase through"


async def test_a_deleted_target_names_itself(kernel):
    """The world moved harder: the agent is GONE. The replay refuses loudly
    with the missing id — never a silent no-op, never stale data."""
    await _wire(kernel)
    assert (await _fire(kernel, "job-1"))["status"] == "completed"

    gone = (
        await kernel.send(
            [
                {
                    "target": "kernel",
                    "payload": {"type": "delete_agent", "id": "adv2_desk"},
                }
            ]
        )
    )[0]
    assert gone["status"] == "completed", gone

    replay = await _fire(kernel, "job-3")
    assert replay["status"] == "failed"
    err = str(replay["result"]).lower()
    assert "adv2_desk" in err, f"the refusal does not name the drift: {replay!r}"
    assert _bought == ["rocket"]


async def test_verb_drift_dies_at_the_verb_scoped_gate(kernel):
    """The tool's surface changed (verb renamed): because grants are
    VERB-scoped (`["buy"]`), the drifted call refuses at the caller's own
    egress — typed, before the handler is even reached. The recording cannot
    quietly exercise a surface it was never granted (measured 2026-08-27:
    egress refusal, not a handler error)."""
    await _wire(kernel)
    assert (await _fire(kernel, "job-1"))["status"] == "completed"

    drifted = await _fire(kernel, "job-4", verb="purchase")  # the verb moved
    assert drifted["status"] == "failed"
    assert drifted["result"]["reason"] == "unauthorized"
    assert drifted["result"]["side"] == "egress"
    assert _bought == ["rocket"]
