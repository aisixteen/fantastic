"""A FAILED HANDOFF IS ANNOUNCED (#177): a reply that could not be delivered is a
`status(done, reason=error)` on the target's inbox, never silence. Twins of the Rust
tests in fantastic-ai-core/src/tests.rs."""

from __future__ import annotations

import asyncio

from _testkit import memory_meta, wire

from ai_core import core
from ai_core.tool_parse import render_call


class _StubKernel:
    """A kernel whose `send` answers every handoff with one fixed outcome."""

    def __init__(self, status: str):
        self.status = status

    async def send(self, calls):
        return [{"pid": None, "status": self.status, "result": {}}]


async def test_only_failed_and_timed_out_count_as_a_failed_handoff(monkeypatch):
    seen: list[tuple] = []

    async def spy(kernel, target, client_id, phase, **detail):
        seen.append((target, client_id, phase, detail))

    monkeypatch.setattr(core, "_emit_status", spy)
    for status in ("failed", "timed_out", "cancelled", "completed"):
        seen.clear()
        await core._deliver(_StubKernel(status), "peer", "t", "c", None)
        if status in ("failed", "timed_out"):
            assert len(seen) == 1, (status, seen)
            target, client_id, phase, detail = seen[0]
            assert (target, client_id, phase) == ("peer", "c", "done")
            assert detail["reason"] == "error"
            assert detail["error"] == status, "the raw status, with no prefix"
        else:
            assert seen == [], (status, seen)


class _Script:
    def __init__(self, turns):
        self.turns = list(turns)
        self.calls = 0

    async def chat(self, messages, max_tokens=None):
        self.calls += 1
        yield self.turns.pop(0) if self.turns else '<call name="done"></call>'


async def test_a_failed_handoff_is_a_status_event_with_detail(
    seeded_kernel, fake_providers
):
    k = seeded_kernel
    aid, leg = "fh_ai", "fh_leg"
    k.create("ram_store.tools", id=leg)
    k.create(
        "ollama_backend.tools",
        id=aid,
        context_window=32768,
        **memory_meta(k, aid, {"cli": "*", "kernel_state": "*", leg: "*"}),
    )
    wire(k, aid, leg)
    wire(k, aid, "kernel_state")
    call = render_call(
        "send",
        {
            "target_id": "kernel_state",
            "payload": {"type": "list_agents"},
            "reply_to": leg,
        },
    )
    fp = fake_providers[aid] = _Script([call])
    k.ctx.ensure_inbox("watch_leg")
    k.watch(leg, "watch_leg")
    try:
        await k.send(
            [
                {
                    "target": aid,
                    "payload": {"type": "send", "text": "go", "client_id": "probe"},
                }
            ]
        )
        q = k.ctx.inboxes["watch_leg"]
        found = None
        for _ in range(250):
            while not q.empty():
                ev = q.get_nowait()
                if (
                    ev.get("type") == "status"
                    and ev.get("phase") == "done"
                    and ev.get("detail", {}).get("reason") == "error"
                ):
                    found = ev
            if found:
                break
            await asyncio.sleep(0.02)
        assert found is not None, "no failed-handoff status event"
        assert "unknown type" in found["detail"]["error"], found
        assert not found["detail"]["error"].startswith("delivery "), found
        assert found["source"] == leg
        assert "ts" in found
        assert found["client_id"] == "probe"
        assert fp.calls == 1
    finally:
        fake_providers.pop(aid, None)
