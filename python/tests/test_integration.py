"""Multi-bundle integration scenarios — running through real Kernel."""

from __future__ import annotations

import json


import ollama_backend.tools as ot


class _FakeProvider:
    def __init__(self, scripts):
        self._scripts = list(scripts)

    async def chat(self, messages, tools):
        items = self._scripts.pop(0) if self._scripts else []
        for x in items:
            yield x


async def test_get_webapp_probe_matrix(seeded_kernel, file_agent):
    """Exactly the webapp agents respond to get_webapp; backends error."""
    # Spin up a representative set
    ob = (
        await seeded_kernel.send(
            "core",
            {
                "type": "create_agent",
                "handler_module": "ollama_backend.tools",
                "file_agent_id": file_agent,
            },
        )
    )["id"]
    ow = (
        await seeded_kernel.send(
            "core",
            {
                "type": "create_agent",
                "handler_module": "ai_chat_webapp.tools",
                "upstream_id": ob,
            },
        )
    )["id"]
    sc = (
        await seeded_kernel.send(
            "core",
            {
                "type": "create_agent",
                "handler_module": "scheduler.tools",
                "file_agent_id": file_agent,
            },
        )
    )["id"]
    cb = (
        await seeded_kernel.send(
            "core",
            {"type": "create_agent", "handler_module": "canvas_backend.tools"},
        )
    )["id"]
    cw = (
        await seeded_kernel.send(
            "core",
            {
                "type": "create_agent",
                "handler_module": "canvas_webapp.tools",
                "upstream_id": cb,
            },
        )
    )["id"]

    expected_webapps = {ow, cw}
    expected_backends = {ob, sc, cb, file_agent}

    for aid in expected_webapps:
        r = await seeded_kernel.send(aid, {"type": "get_webapp"})
        assert "url" in r, f"{aid} should respond as webapp; got {r}"
        assert "error" not in r

    for aid in expected_backends:
        r = await seeded_kernel.send(aid, {"type": "get_webapp"})
        assert "error" in r, f"{aid} should NOT respond as webapp; got {r}"


async def test_persistence_routing_through_file_agent(
    seeded_kernel, file_agent, tmp_path
):
    """ollama_backend.send writes per-client chat (default chat_cli.json) via the file agent at the right path."""
    ob = (
        await seeded_kernel.send(
            "core",
            {
                "type": "create_agent",
                "handler_module": "ollama_backend.tools",
                "file_agent_id": file_agent,
            },
        )
    )["id"]
    ot._providers[ob] = _FakeProvider([["hello"]])
    try:
        await seeded_kernel.send(ob, {"type": "send", "text": "world"})
    finally:
        ot._providers.pop(ob, None)
    chat = tmp_path / ".fantastic" / "agents" / ob / "chat_cli.json"
    assert chat.exists()
    data = json.loads(chat.read_text())
    assert data[-2]["content"] == "world"
    assert data[-1]["content"] == "hello"


async def test_inter_llm_tool_call(seeded_kernel, file_agent):
    """ollama_A's tool_call to ollama_B routes through kernel.send and returns to A."""
    a = (
        await seeded_kernel.send(
            "core",
            {
                "type": "create_agent",
                "handler_module": "ollama_backend.tools",
                "file_agent_id": file_agent,
            },
        )
    )["id"]
    b = (
        await seeded_kernel.send(
            "core",
            {
                "type": "create_agent",
                "handler_module": "ollama_backend.tools",
                "file_agent_id": file_agent,
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


async def test_scheduler_fires_through_kernel_send(seeded_kernel, file_agent):
    """A scheduled job fires kernel.send to its target; target gets the payload."""
    sid = (
        await seeded_kernel.send(
            "core",
            {
                "type": "create_agent",
                "handler_module": "scheduler.tools",
                "file_agent_id": file_agent,
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


async def test_canvas_same_bundle_exclusion_via_handler_module(seeded_kernel):
    """The canvas's same-bundle exclusion is in JS, but verify the records have the
    handler_module field that drives the filter."""
    cw_a = (
        await seeded_kernel.send(
            "core",
            {
                "type": "create_agent",
                "handler_module": "canvas_webapp.tools",
                "upstream_id": "x",
            },
        )
    )["id"]
    cw_b = (
        await seeded_kernel.send(
            "core",
            {
                "type": "create_agent",
                "handler_module": "canvas_webapp.tools",
                "upstream_id": "y",
            },
        )
    )["id"]
    a_rec = seeded_kernel.get(cw_a)
    b_rec = seeded_kernel.get(cw_b)
    # Same handler_module → JS-side filter excludes them from each other's iframes.
    assert a_rec["handler_module"] == b_rec["handler_module"]
