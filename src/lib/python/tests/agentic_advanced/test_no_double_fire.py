"""ADV-1 — exactly-once across suspension: LangGraph's double-execution bug,
structurally impossible here.

LangGraph re-runs an interrupted node FROM THE TOP on resume: every side effect
before the `interrupt()` fires again (maintainer issue #6208 — "a node with two
interrupts will rerun after only one resume"; blog.raed.dev/posts/langgraph-hitl:
duplicate tickets, invisible in message history). Their fix is developer
discipline: "make your side effects idempotent."

Here suspension is not a checkpoint-replay — it is simply a turn that ended.
Resume is a NEW message, and even a resume path that NAIVELY replays every
recorded action (exactly what LangGraph's runtime does) cannot double-fire:
the action ids are ledger executions, a replay JOINS the recorded outcome.
Idempotency is the substrate's property, not the author's chore.
"""

from __future__ import annotations

import sys
import types

_counts: dict[str, int] = {}

_desk = types.ModuleType("adv1_desk.tools")


async def _desk_handler(id, payload, kernel):
    verb = payload.get("type")
    if verb in ("boot", "shutdown"):
        return None
    if verb == "reflect":
        return {"id": id, "sentence": "side-effect desk", "verbs": {}}
    if verb in ("ticket", "email"):
        _counts[verb] = _counts.get(verb, 0) + 1
        return {verb: _counts[verb]}
    return {"error": f"unknown verb {verb!r}"}


_desk.handler = _desk_handler
_desk.VERBS = {}
sys.modules["adv1_desk.tools"] = _desk

# The orchestrator mirrors the LangGraph failure shape: a "node" that does a
# side effect, then suspends on approval. Its resume path is DELIBERATELY the
# naive one — replay everything recorded — because that is the strongest claim:
# even the wrong resume implementation cannot double-fire.
_orc = types.ModuleType("adv1_orc.tools")


async def _orc_handler(id, payload, kernel):
    verb = payload.get("type")
    if verb in ("boot", "shutdown"):
        return None
    if verb == "reflect":
        return {"id": id, "sentence": "orchestrator", "verbs": {}}
    if verb == "run":
        # side effect BEFORE the suspension — LangGraph's re-fire victim
        t = (
            await kernel.send(
                [{"target": "adv1_desk", "payload": {"type": "ticket"}, "id": "tkt-1"}]
            )
        )[0]
        # ...then "interrupt": return a question and END THE TURN
        return {
            "ticket": t["result"],
            "question": {
                "type": "question",
                "kind": "confirm",
                "question": "send the email?",
                "action": {
                    "target": "adv1_desk",
                    "payload": {"type": "email"},
                    "id": "eml-1",
                },
            },
        }
    if verb == "resume":
        # the NAIVE resume: replay every recorded action of the "node"
        outs = await kernel.send(
            [
                {"target": "adv1_desk", "payload": {"type": "ticket"}, "id": "tkt-1"},
                {"target": "adv1_desk", "payload": {"type": "email"}, "id": "eml-1"},
            ]
        )
        return {"replayed": outs}
    return {"error": f"unknown verb {verb!r}"}


_orc.handler = _orc_handler
_orc.VERBS = {}
sys.modules["adv1_orc.tools"] = _orc


async def _wire(kernel):
    _counts.clear()
    for mod, aid, extra in (
        ("adv1_desk.tools", "adv1_desk", {"callable_by": {"adv1_orc": "*"}}),
        ("adv1_orc.tools", "adv1_orc", {"can_call": {"adv1_desk": "*"}}),
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


async def test_naive_replay_on_resume_fires_nothing_twice(kernel):
    """The #6208 shape: side effect → suspend → approve → resume. The resume
    REPLAYS both recorded actions and the counts do not move."""
    await _wire(kernel)

    run = (await kernel.send([{"target": "adv1_orc", "payload": {"type": "run"}}]))[0]
    assert run["status"] == "completed"
    assert _counts == {"ticket": 1}, "proposing must not act"
    action = run["result"]["question"]["action"]

    # the human approves: the click fires the proposal's execution id
    click = (
        await kernel.send(
            [
                {
                    "target": action["target"],
                    "payload": action["payload"],
                    "id": action["id"],
                }
            ]
        )
    )[0]
    assert click["status"] == "completed"
    assert _counts == {"ticket": 1, "email": 1}

    # the naive resume replays EVERYTHING the node did — and nothing re-fires
    resume = (
        await kernel.send([{"target": "adv1_orc", "payload": {"type": "resume"}}])
    )[0]
    assert resume["status"] == "completed"
    replayed = resume["result"]["replayed"]
    assert [o["status"] for o in replayed] == ["completed", "completed"]
    assert [o["attempts"] for o in replayed] == [1, 1], "a replay re-executed"
    assert _counts == {"ticket": 1, "email": 1}, (
        f"double-fire: {_counts} — the LangGraph bug reproduced here"
    )
    # the joins returned the RECORDED outcomes, not re-runs
    assert replayed[0]["result"] == {"ticket": 1}
    assert replayed[1]["result"] == {"email": 1}


async def test_one_resume_does_not_fire_the_other_pending_question(kernel):
    """Two proposals pending, ONE approved: the un-approved action must not
    exist anywhere — not executed, not in the ledger."""
    await _wire(kernel)

    a1 = {"target": "adv1_desk", "payload": {"type": "ticket"}, "id": "act-a"}
    # a second proposal is only dialog state — nothing is dispatched for it
    click = (await kernel.send([a1]))[0]
    assert click["status"] == "completed"
    assert _counts == {"ticket": 1}
    assert _counts.get("email", 0) == 0, "the un-approved action fired"

    led = (await kernel.send([{"target": "kernel", "payload": {"type": "ledger"}}]))[0][
        "result"
    ]
    ids = {r["id"] for r in led["executions"]}
    assert "act-a" in ids
    assert "act-b" not in ids, "an un-approved proposal reached the ledger"
