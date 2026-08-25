"""Multi-bundle integration scenarios — running through real Kernel."""

from __future__ import annotations

import ollama_backend.tools as ot


class _FakeProvider:
    def __init__(self, scripts):
        self._scripts = list(scripts)

    async def chat(self, messages):
        items = self._scripts.pop(0) if self._scripts else []
        for x in items:
            yield x


async def test_persistence_routing_through_loader(seeded_kernel, store_agent):
    """ollama_backend.send persists its per-client chat THROUGH the loader (a mounted
    chat yaml_state — the AI writes no disk itself); read it back via the `history` verb."""
    ob = (
        (
            await seeded_kernel.send(
                [
                    {
                        "target": "kernel_state",
                        "payload": {
                            "type": "create_agent",
                            "parent_id": "kernel_state",
                            "handler_module": "ollama_backend.tools",
                            "file_bridge_id": store_agent,
                            # No explicit grant needed: the chat persists THROUGH the loader
                            # ($loader baseline) into a `chat` child ($children baseline).
                        },
                    }
                ]
            )
        )[0]["result"]
    )["id"]
    ot._providers[ob] = _FakeProvider([["hello"]])
    try:
        (
            await seeded_kernel.send(
                [{"target": ob, "payload": {"type": "send", "text": "world"}}]
            )
        )[0]["result"]
    finally:
        ot._providers.pop(ob, None)
    data = (
        (await seeded_kernel.send([{"target": ob, "payload": {"type": "history"}}]))[0][
            "result"
        ]
    )["messages"]
    assert data[-2]["content"] == "world"
    assert data[-1]["content"] == "hello"


# NOTE: inter-LLM tool-calling (A calls B) is no longer a BACKEND concern — the
# backend does one bare pass, and orchestrating one AI to call another is the
# emergent MIND's job (a script driving the tool loop). Proven by the mind tests
# in each runtime, not here.


async def test_scheduler_fires_through_kernel_send(seeded_kernel, store_agent):
    """A scheduled job fires kernel.send to its target; target gets the payload."""
    sid = (
        (
            await seeded_kernel.send(
                [
                    {
                        "target": "kernel_state",
                        "payload": {
                            "type": "create_agent",
                            "parent_id": "kernel_state",
                            "handler_module": "scheduler.tools",
                            # Deny-all: firing at a target is lateral egress, so the wiring is
                            # an EXPLICIT grant — the scheduler may `say` to `cli`, nothing more.
                            "can_call": {"cli": ["say"]},
                        },
                    }
                ]
            )
        )[0]["result"]
    )["id"]
    # Two-sided deny-all: open cli's reciprocal ingress for the scheduler's `say`.
    cli = seeded_kernel.ctx.agents.get("cli")
    cb = dict(cli._meta.get("callable_by") or {})
    cb[sid] = ["say"]
    (
        await seeded_kernel.send(
            [
                {
                    "target": "kernel_state",
                    "payload": {"type": "update_agent", "id": "cli", "callable_by": cb},
                }
            ]
        )
    )[0]["result"]
    sch = (
        await seeded_kernel.send(
            [
                {
                    "target": sid,
                    "payload": {
                        "type": "schedule",
                        "target": "cli",
                        "payload": {
                            "type": "say",
                            "text": "tick-via-scheduler",
                            "source": sid,
                        },
                        "interval_seconds": 60,
                    },
                }
            ]
        )
    )[0]["result"]
    (
        await seeded_kernel.send(
            [
                {
                    "target": sid,
                    "payload": {"type": "tick_now", "schedule_id": sch["schedule_id"]},
                }
            ]
        )
    )[0]["result"]

    # cli's inbox got the say payload ONLY — one delivery per tick. The
    # `schedule_fired` telemetry rides the scheduler's own inbox, not the target's
    # (the target-echo doubled every delivery to a live program's on_message).
    q = seeded_kernel.ctx.ensure_inbox("cli")
    msgs = []
    while not q.empty():
        msgs.append(q.get_nowait())
    types = [m["type"] for m in msgs]
    assert "say" in types
    assert "schedule_fired" not in types


async def test_records_carry_handler_module(seeded_kernel):
    """Records carry the handler_module field that the TS canvas keys off
    to pick a view (inline by handler_module, else iframe via get_webapp)."""
    a = (
        (
            await seeded_kernel.send(
                [
                    {
                        "target": "kernel",
                        "payload": {
                            "type": "create_agent",
                            "parent_id": "kernel_state",
                            "handler_module": "frontend_view.tools",
                        },
                    }
                ]
            )
        )[0]["result"]
    )["id"]
    b = (
        (
            await seeded_kernel.send(
                [
                    {
                        "target": "kernel",
                        "payload": {
                            "type": "create_agent",
                            "parent_id": "kernel_state",
                            "handler_module": "frontend_view.tools",
                        },
                    }
                ]
            )
        )[0]["result"]
    )["id"]
    a_rec = seeded_kernel.get(a)
    b_rec = seeded_kernel.get(b)
    assert a_rec["handler_module"] == b_rec["handler_module"] == "frontend_view.tools"
