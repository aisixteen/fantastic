"""Agent send/emit/watch routing.

These tests verify the message routing surface that bundle handlers
rely on (`agent.send`, `agent.emit`, `agent.watch`/`unwatch`). Routing
is flat-global — `send(any_id, ...)` resolves through `ctx.agents`.
"""

from __future__ import annotations
# twin: unknown_agent_carries_the_live_index
# twin: unknown_agent_error_is_python_repr
# twin: send_to_missing_target_returns_error


async def test_send_to_missing_agent_returns_error(kernel):
    """The error names the LIVE ids, because a bare `no agent 'x'` teaches the caller
    nothing. Reading the flat index is a dict lookup, not a reflect — see
    `Kernel.unknown_agent`."""
    r = (await kernel.send([{"target": "nope", "payload": {"type": "reflect"}}]))[0][
        "result"
    ]
    assert r["error"] == "no agent 'nope'"
    assert r["agents"] == sorted(kernel.ctx.agents)
    assert r["agent_count"] == len(kernel.ctx.agents)
    assert "hint" not in r  # a plain unknown id is not a placeholder


async def test_a_placeholder_id_is_named_as_one(kernel):
    """A model that copies `<id>` out of the prompt's own example must be told it is a
    placeholder, not just `no agent '<id>'`. Angle brackets
    are structurally a SLOT — no id this kernel mints can contain them."""
    r = (await kernel.send([{"target": "<id>", "payload": {"type": "reflect"}}]))[0][
        "result"
    ]
    assert r["error"] == "no agent '<id>'"
    assert "PLACEHOLDER" in r["hint"]
    assert "agents" in r


async def test_send_with_bad_handler_module_returns_error(kernel):
    kernel.create("nonexistent.module.path", id="broken")
    r = (await kernel.send([{"target": "broken", "payload": {"type": "reflect"}}]))[0][
        "result"
    ]
    assert "error" in r
    assert "import" in r["error"].lower() or "no module" in r["error"].lower()


async def test_send_system_verb_answered_natively(seeded_kernel):
    """`list_agents` is a system verb every Agent answers natively. Returns flat
    all-records."""
    r = (
        await seeded_kernel.send(
            [{"target": "kernel_state", "payload": {"type": "list_agents"}}]
        )
    )[0]["result"]
    assert "agents" in r
    ids = {a["id"] for a in r["agents"]}
    assert "kernel_state" in ids
    assert "cli" in ids


async def test_emit_puts_on_inbox(kernel):
    kernel.create("file_bridge.tools", allow=["."], id="t")
    q = kernel.ctx.ensure_inbox("t")  # the test is the reader
    await kernel.emit("t", {"type": "hello", "n": 1})
    msg = q.get_nowait()
    assert msg["n"] == 1


async def test_watch_then_emit_fans_payload_to_watcher(kernel):
    kernel.create("file_bridge.tools", allow=["."], id="src")
    kernel.create("file_bridge.tools", allow=["."], id="watcher")
    kernel.ctx.ensure_inbox("watcher")
    kernel.watch("src", "watcher")
    await kernel.emit("src", {"type": "x", "n": 42})
    # watcher's inbox got the mirror.
    assert kernel.ctx.inboxes["watcher"].get_nowait()["n"] == 42


async def test_unwatch_stops_fanout(kernel):
    kernel.create("file_bridge.tools", allow=["."], id="src")
    kernel.create("file_bridge.tools", allow=["."], id="w")
    kernel.ctx.ensure_inbox("w")
    kernel.watch("src", "w")
    kernel.unwatch("src", "w")
    await kernel.emit("src", {"type": "x"})
    assert kernel.ctx.ensure_inbox("w").empty()


async def test_send_payload_reaches_the_targets_own_registered_inbox(seeded_kernel):
    """`send` fans the payload out (drops on inbox) before invoking
    the handler — so watchers / state subscribers see traffic events."""
    q = seeded_kernel.ctx.ensure_inbox("kernel_state")  # the test is the reader
    (
        await seeded_kernel.send(
            [{"target": "kernel_state", "payload": {"type": "list_agents"}}]
        )
    )[0]["result"]
    msg = q.get_nowait()
    assert msg["type"] == "list_agents"
