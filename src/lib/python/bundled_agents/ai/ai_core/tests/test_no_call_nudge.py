"""The no-call nudge — one runtime reminder when a CHAIN turn dispatched nothing.

Measured 2026-08-25 (`live_subai` / `live_subai_graph`, qwen3.8-27b, twice each):
mid-task the model wrote "Let me send {type:'list_agents'} to kernel" INSIDE its
thinking, ended the completion without the envelope, and the no-call turn ended
the chain — runs whose gate, premise and delivery were all clean died on a single
lapse. The runtime now says "continue" once, the way a human would.

The cap is structural, not counted: the nudge is itself the next turn's incoming
text, and a turn answering the nudge is never nudged again.
"""

from __future__ import annotations

import asyncio
import json

from ai_core.core import _NO_CALL_NUDGE, _route_reply


class _Kernel:
    def __init__(self):
        self.delivered: list[tuple[str, str]] = []

    def get(self, _id):
        return {}

    async def send(self, items):
        outs = []
        for it in items:
            if it["payload"].get("type") == "send":
                self.delivered.append((it["target"], it["payload"]["text"]))
                outs.append({"pid": None, "status": "completed", "result": None})
            else:
                outs.append(
                    {"pid": f"{it['target']}:1", "status": "completed", "result": {}}
                )
        return outs


async def _settle(k, want, timeout=2.0):
    t0 = asyncio.get_event_loop().time()
    while len(k.delivered) < want:
        if asyncio.get_event_loop().time() - t0 > timeout:
            break
        await asyncio.sleep(0.005)


REPLY_IN = '[reply to ks:1 · kernel_state reflect]\n{"id": "kernel_state"}'
THINK_ONLY = "<think>Let me send {type:'list_agents'} to kernel.</think>"


async def test_a_chain_turn_with_no_call_gets_one_nudge():
    k = _Kernel()
    await _route_reply("ai", k, THINK_ONLY, "cli", None, None, 180.0, REPLY_IN)
    await _settle(k, 1)
    assert k.delivered == [("ai", _NO_CALL_NUDGE)]


async def test_the_nudge_is_never_answered_with_another_nudge():
    """Second consecutive lapse → the chain ends exactly as before."""
    k = _Kernel()
    await _route_reply("ai", k, THINK_ONLY, "cli", None, None, 180.0, _NO_CALL_NUDGE)
    await asyncio.sleep(0.05)
    assert k.delivered == []


async def test_a_chat_turn_is_never_nudged():
    """Prose to a person is the product, not a lapse."""
    k = _Kernel()
    await _route_reply(
        "ai", k, "The weather is nice.", "cli", None, None, 180.0, "how is it?"
    )
    await asyncio.sleep(0.05)
    assert k.delivered == []


async def test_a_chain_turn_that_acted_is_not_nudged():
    k = _Kernel()
    call = (
        '<tool_call>{"name":"send","arguments":{"target_id":"kernel_state",'
        '"payload":{"type":"reflect"},"reply_to":"self"}}</tool_call>'
    )
    await _route_reply("ai", k, call, "cli", None, None, 180.0, REPLY_IN)
    await _settle(k, 1)
    assert len(k.delivered) == 1
    assert k.delivered[0][1].startswith("[reply to ")


async def test_a_fire_and_forget_only_turn_is_not_nudged():
    """It ACTED — discarding the answer was explicit."""
    k = _Kernel()
    call = (
        '<tool_call>{"name":"send","arguments":{"target_id":"totals",'
        '"payload":{"type":"set","key":"k","value":1},"reply_to":"null"}}</tool_call>'
    )
    await _route_reply("ai", k, call, "cli", None, None, 180.0, REPLY_IN)
    await asyncio.sleep(0.05)
    assert k.delivered == []
