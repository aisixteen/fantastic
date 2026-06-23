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
        await seeded_kernel.send(
            "kernel_state",
            {
                "type": "create_agent",
                "handler_module": "ollama_backend.tools",
                "file_bridge_id": store_agent,
            },
        )
    )["id"]
    ot._providers[ob] = _FakeProvider([["hello"]])
    try:
        await seeded_kernel.send(ob, {"type": "send", "text": "world"})
    finally:
        ot._providers.pop(ob, None)
    data = (await seeded_kernel.send(ob, {"type": "history"}))["messages"]
    assert data[-2]["content"] == "world"
    assert data[-1]["content"] == "hello"


async def test_inter_llm_tool_call(seeded_kernel, store_agent):
    """ollama_A's tool_call to ollama_B routes through kernel.send and returns to A."""
    a = (
        await seeded_kernel.send(
            "kernel_state",
            {
                "type": "create_agent",
                "handler_module": "ollama_backend.tools",
                "file_bridge_id": store_agent,
            },
        )
    )["id"]
    b = (
        await seeded_kernel.send(
            "kernel_state",
            {
                "type": "create_agent",
                "handler_module": "ollama_backend.tools",
                "file_bridge_id": store_agent,
            },
        )
    )["id"]

    # A scripts: emit tool_call to B; then summary text.
    ot._providers[a] = _FakeProvider(
        [
            [
                {
                    "tool_call": {
                        "id": "c1",
                        "name": "send",
                        "arguments": {
                            "target_id": b,
                            "payload": {
                                "type": "send",
                                "text": "what color is the sky?",
                            },
                        },
                    }
                }
            ],
            ["The other agent says: blue."],
        ]
    )
    # B answers with text directly.
    ot._providers[b] = _FakeProvider([["The sky is blue."]])

    try:
        r = await seeded_kernel.send(a, {"type": "send", "text": "ask B"})
        assert "blue" in r["final"].lower()
    finally:
        ot._providers.pop(a, None)
        ot._providers.pop(b, None)


async def test_scheduler_fires_through_kernel_send(seeded_kernel, store_agent):
    """A scheduled job fires kernel.send to its target; target gets the payload."""
    sid = (
        await seeded_kernel.send(
            "kernel_state",
            {
                "type": "create_agent",
                "handler_module": "scheduler.tools",
            },
        )
    )["id"]
    sch = await seeded_kernel.send(
        sid,
        {
            "type": "schedule",
            "target": "cli",
            "payload": {"type": "say", "text": "tick-via-scheduler", "source": sid},
            "interval_seconds": 60,
        },
    )
    await seeded_kernel.send(
        sid, {"type": "tick_now", "schedule_id": sch["schedule_id"]}
    )

    # cli's inbox got the say payload + schedule_fired event
    q = seeded_kernel._ensure_inbox("cli")
    msgs = []
    while not q.empty():
        msgs.append(q.get_nowait())
    types = [m["type"] for m in msgs]
    assert "say" in types
    assert "schedule_fired" in types


async def test_records_carry_handler_module(seeded_kernel):
    """Records carry the handler_module field that the TS canvas keys off
    to pick a view (inline by handler_module, else iframe via get_webapp)."""
    a = (
        await seeded_kernel.send(
            "kernel_state",
            {"type": "create_agent", "handler_module": "frontend_view.tools"},
        )
    )["id"]
    b = (
        await seeded_kernel.send(
            "kernel_state",
            {"type": "create_agent", "handler_module": "frontend_view.tools"},
        )
    )["id"]
    a_rec = seeded_kernel.get(a)
    b_rec = seeded_kernel.get(b)
    assert a_rec["handler_module"] == b_rec["handler_module"] == "frontend_view.tools"
