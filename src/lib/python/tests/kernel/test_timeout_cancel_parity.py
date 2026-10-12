"""A dying task registers no new work.

`cancel_task` is two-phase: every bundle's `on_cancel` hook runs first (real
teardown — an OS child, a PTY, a socket), then the asyncio.Tasks are aborted.
Phase ① AWAITS, so a handler inside the doomed subtree gets scheduling passes
between the decision to kill it and its abort.

Measured 2026-09-02: work it sent in that window started fresh dispatches — new
`ps` rows, new OS children — that outlived the cancel that was supposed to take
the whole group down. The subtree is now MARKED before phase ①, and a send from a
marked task settles `cancelled` without dispatching.
"""

from __future__ import annotations

import asyncio

from kernel._kernel import CANCELLED


async def _mk(kernel, agent_id, module="file_bridge.tools", **meta):
    payload = {
        "type": "create_agent",
        "parent_id": kernel.id,
        "id": agent_id,
        "handler_module": module,
        "ingress_rule": "allow_all_ingress",
        **meta,
    }
    rec = (await kernel.send([{"target": kernel.id, "payload": payload}]))[0]["result"]
    assert rec.get("id") == agent_id, rec
    return agent_id


async def test_a_send_from_a_killed_task_does_not_dispatch(seeded_kernel):
    """Drive the guard directly: mark the current task as `cancel_task` does,
    then send. The item must settle `cancelled` and reach no handler."""
    victim = await _mk(seeded_kernel, "tc_victim")
    reached: list[str] = []

    async def _spy(agent_id, payload, agent):
        reached.append(payload.get("type", ""))
        return {"ok": True}

    import file_bridge.tools as fb

    original = fb.handler
    fb.handler = _spy
    try:
        # A live, REGISTERED task whose entry we mark exactly as cancel_task
        # does before it aborts anything.
        async def _body():
            from kernel._kernel import _current_task

            _current_task.set("tc_probe")
            seeded_kernel.ctx.register_task(
                "tc_probe",
                seeded_kernel.id,
                "probe",
                asyncio.current_task(),
                parent=None,
            )
            seeded_kernel.ctx.tasks["tc_probe"]["_killed"] = True
            return (
                await seeded_kernel.send(
                    [{"target": victim, "payload": {"type": "list", "path": ""}}]
                )
            )[0]

        out = await asyncio.ensure_future(_body())
    finally:
        fb.handler = original

    assert out["status"] == CANCELLED, out
    assert out["result"] == {"cancelled": True}, out
    assert out["pid"] is None, out
    assert reached == [], "the handler must never run for a killed sender"


async def test_a_live_task_still_sends_normally(seeded_kernel):
    """The guard must not fire for an ordinary in-flight dispatch."""
    victim = await _mk(seeded_kernel, "tc_live")

    async def _body():
        from kernel._kernel import _current_task

        _current_task.set("tc_live_probe")
        seeded_kernel.ctx.register_task(
            "tc_live_probe",
            seeded_kernel.id,
            "probe",
            asyncio.current_task(),
            parent=None,
        )
        return (
            await seeded_kernel.send(
                [{"target": victim, "payload": {"type": "list", "path": ""}}]
            )
        )[0]

    out = await asyncio.ensure_future(_body())
    assert out["status"] != CANCELLED, out
