"""Kernel send/emit/watch routing."""

from __future__ import annotations


async def test_send_to_missing_agent_returns_error(kernel):
    r = await kernel.send("nope", {"type": "reflect"})
    assert r == {"error": "no agent 'nope'"}


async def test_send_with_bad_handler_module_returns_error(kernel):
    kernel.create("nonexistent.module.path", id="broken")
    r = await kernel.send("broken", {"type": "reflect"})
    assert "error" in r
    assert "import" in r["error"].lower() or "no module" in r["error"].lower()


async def test_send_routes_to_handler(seeded_kernel):
    r = await seeded_kernel.send("core", {"type": "reflect"})
    assert "verbs" in r
    assert "list_agents" in r["verbs"]


async def test_emit_puts_on_inbox(kernel):
    kernel.create("core.tools", id="t")
    await kernel.emit("t", {"type": "hello", "n": 1})
    q = kernel._ensure_inbox("t")
    msg = q.get_nowait()
    assert msg["n"] == 1


async def test_emit_fans_out_to_watchers(kernel):
    kernel.create("core.tools", id="src")
    kernel.create("core.tools", id="watcher")
    kernel.watch("src", "watcher")
    await kernel.emit("src", {"type": "x", "n": 42})
    assert kernel._ensure_inbox("watcher").get_nowait()["n"] == 42


async def test_unwatch_stops_routing(kernel):
    kernel.create("core.tools", id="src")
    kernel.create("core.tools", id="w")
    kernel.watch("src", "w")
    kernel.unwatch("src", "w")
    await kernel.emit("src", {"type": "x"})
    assert kernel._ensure_inbox("w").empty()


async def test_send_also_fanouts_to_inbox(seeded_kernel):
    """`send` fans the payload out before invoking the handler."""
    await seeded_kernel.send("core", {"type": "reflect"})
    q = seeded_kernel._ensure_inbox("core")
    # The payload is on core's own inbox after the send
    msg = q.get_nowait()
    assert msg["type"] == "reflect"
