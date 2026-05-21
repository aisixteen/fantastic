"""Agent send/emit/watch routing.

These tests verify the message routing surface that bundle handlers
rely on (`agent.send`, `agent.emit`, `agent.watch`/`unwatch`). Routing
is flat-global — `send(any_id, ...)` resolves through `ctx.agents`.
"""

from __future__ import annotations


async def test_send_to_missing_agent_returns_error(kernel):
    r = await kernel.send("nope", {"type": "reflect"})
    assert r == {"error": "no agent 'nope'"}


async def test_send_with_bad_handler_module_returns_error(kernel):
    kernel.create("nonexistent.module.path", id="broken")
    r = await kernel.send("broken", {"type": "reflect"})
    assert "error" in r
    assert "import" in r["error"].lower() or "no module" in r["error"].lower()


async def test_send_system_verb_answered_natively(seeded_kernel):
    """`list_agents` is a system verb every Agent answers natively
    (used to live on the `core` bundle). Returns flat all-records."""
    r = await seeded_kernel.send("core", {"type": "list_agents"})
    assert "agents" in r
    ids = {a["id"] for a in r["agents"]}
    assert "core" in ids
    assert "cli" in ids


async def test_emit_puts_on_inbox(kernel):
    kernel.create("file.tools", id="t")
    await kernel.emit("t", {"type": "hello", "n": 1})
    q = kernel.ctx.inboxes["t"]
    msg = q.get_nowait()
    assert msg["n"] == 1


async def test_emit_fans_out_to_watchers(kernel):
    kernel.create("file.tools", id="src")
    kernel.create("file.tools", id="watcher")
    kernel.watch("src", "watcher")
    await kernel.emit("src", {"type": "x", "n": 42})
    # watcher's inbox got the mirror.
    assert kernel.ctx.inboxes["watcher"].get_nowait()["n"] == 42


async def test_unwatch_stops_routing(kernel):
    kernel.create("file.tools", id="src")
    kernel.create("file.tools", id="w")
    kernel.watch("src", "w")
    kernel.unwatch("src", "w")
    await kernel.emit("src", {"type": "x"})
    assert kernel.ctx.inboxes["w"].empty()


async def test_send_also_fanouts_to_inbox(seeded_kernel):
    """`send` fans the payload out (drops on inbox) before invoking
    the handler — so watchers / state subscribers see traffic events."""
    await seeded_kernel.send("core", {"type": "list_agents"})
    q = seeded_kernel.ctx.inboxes["core"]
    msg = q.get_nowait()
    assert msg["type"] == "list_agents"
