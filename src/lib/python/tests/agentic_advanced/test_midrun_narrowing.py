"""ADV-3 — mid-run narrowing: the operator reshapes a RUNNING agent's reach.

Octomind ripped LangGraph out because they could not inspect or modify an
agent's tool set based on what the agent had already discovered — the graph is
compiled before the run and the run owns it.

Here authority is table rows, not compiled topology: `update_agent` rewrites
`can_call` on a live agent between two of its steps, the very next step is
refused, and the refusal returns INTO the agent's own message as a typed outcome
it can react to. No restart, no recompile, no new process.
"""

from __future__ import annotations

import types
from kernel import register_unlisted

_steps: list[dict] = []

_desk = types.ModuleType("adv3_desk.tools")


async def _desk_handler(id, payload, kernel):
    verb = payload.get("type")
    if verb in ("boot", "shutdown"):
        return None
    if verb == "reflect":
        return {"id": id, "sentence": "tool desk", "verbs": {}}
    if verb == "use":
        return {"used": True}
    return {"error": f"unknown verb {verb!r}"}


_desk.handler = _desk_handler
_desk.VERBS = {}
register_unlisted("adv3_desk.tools", _desk)

_worker = types.ModuleType("adv3_worker.tools")


async def _worker_handler(id, payload, kernel):
    verb = payload.get("type")
    if verb in ("boot", "shutdown"):
        return None
    if verb == "reflect":
        return {"id": id, "sentence": "long-running worker", "verbs": {}}
    if verb == "step":
        out = (
            await kernel.send([{"target": "adv3_desk", "payload": {"type": "use"}}])
        )[0]
        _steps.append(out)  # what the WORKER saw, inside its own message
        return out
    return {"error": f"unknown verb {verb!r}"}


_worker.handler = _worker_handler
_worker.VERBS = {}
register_unlisted("adv3_worker.tools", _worker)


async def test_revoking_a_tool_mid_run_is_immediate_and_model_visible(kernel):
    _steps.clear()
    for mod, aid, extra in (
        ("adv3_desk.tools", "adv3_desk", {"callable_by": {"adv3_worker": ["use"]}}),
        ("adv3_worker.tools", "adv3_worker", {"can_call": {"adv3_desk": ["use"]}}),
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

    # step 1: the tool is in reach
    s1 = (await kernel.send([{"target": "adv3_worker", "payload": {"type": "step"}}]))[
        0
    ]
    assert s1["status"] == "completed" and s1["result"]["result"] == {"used": True}

    # the operator NARROWS the running worker — egress rewritten live
    up = (
        await kernel.send(
            [
                {
                    "target": "kernel",
                    "payload": {
                        "type": "update_agent",
                        "id": "adv3_worker",
                        "can_call": {},
                    },
                }
            ]
        )
    )[0]
    assert up["status"] == "completed", up

    # step 2: the very next call dies at the worker's OWN egress...
    s2 = (await kernel.send([{"target": "adv3_worker", "payload": {"type": "step"}}]))[
        0
    ]
    assert s2["status"] == "completed"  # the worker's message ran — and SAW the refusal
    refusal = s2["result"]
    assert refusal["status"] == "failed"
    assert refusal["result"]["reason"] == "unauthorized"
    assert refusal["result"]["side"] == "egress"
    assert "adv3_worker" in refusal["result"]["error"]
    assert "adv3_desk" in refusal["result"]["error"]

    # ...and the refusal arrived INSIDE the worker's message (reactable), not
    # swallowed by the substrate
    assert len(_steps) == 2
    assert _steps[1]["result"]["reason"] == "unauthorized"
