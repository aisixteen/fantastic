"""Every call in a turn gets its reply — delivered TOGETHER, as one message.

Two defects pinned here, both measured live:

2026-08-24 (`live_build`, qwen3.8-27b): replies were delivered SEQUENTIALLY and
each delivery blocked on a whole model turn — 26 of 36 calls never answered.

2026-08-25 (`live_subai_langgraph`, qwen3.8-27b): one reply per turn — a turn
that made 7 calls spent the next 7 turns collecting its own answers (`1 of 7`
… `7 of 7`), asked `list_agents` at t3, received the t1 reflect instead, and
re-asked until the repeat tripwire killed the run with nothing built.

The contract now: ALL of one turn's results arrive as ONE delivered message
per `reply_to` destination — one `[reply to …]` block per call, in emission
order. The model never sees a partial batch, so it never re-asks.
"""

from __future__ import annotations

import asyncio
import json

from ai_core.core import _route_reply


def _call(target: str, verb: str = "reflect", reply_to: str = "self") -> str:
    return (
        "<tool_call>"
        + json.dumps(
            {
                "name": "send",
                "arguments": {
                    "target_id": target,
                    "payload": {"type": verb},
                    "reply_to": reply_to,
                },
            }
        )
        + "</tool_call>"
    )


class _Kernel:
    """Array-shaped stub: one outcome per item, pid minted at dispatch. `sent`
    records deliveries (the `type: send` handoffs to the AI agent); `batches`
    records each kernel batch as the list of its targets."""

    def __init__(self):
        self.sent: list[tuple[str, dict]] = []
        self.batches: list[list[str]] = []

    def get(self, _id):
        return {}

    async def send(self, items):
        outs = []
        if any(i["payload"].get("type") != "send" for i in items):
            self.batches.append([i["target"] for i in items])
        for it in items:
            if it["payload"].get("type") == "send":
                self.sent.append((it["target"], it["payload"]))
                outs.append({"pid": None, "status": "completed", "result": None})
            else:
                outs.append(
                    {
                        "pid": f"{it['target']}:pid",
                        "status": "completed",
                        "result": {"ok": it["target"]},
                    }
                )
        return outs


async def _settle(k: _Kernel, want: int, timeout: float = 2.0) -> None:
    """Deliveries are spawned, not awaited — wait for them to land."""
    began = asyncio.get_event_loop().time()
    while len(k.sent) < want:
        if asyncio.get_event_loop().time() - began > timeout:
            raise AssertionError(f"delivered {len(k.sent)}, wanted {want}")
        await asyncio.sleep(0.005)


async def test_a_turn_of_four_calls_is_answered_in_one_message():
    """Four calls out → ONE delivered message holding four tagged blocks, in
    the model's own emission order."""
    k = _Kernel()
    text = _call("kernel") + _call("readings") + _call("totals") + _call("runner")
    await _route_reply("ai", k, text, "cli", None, None, 180.0, "")
    await _settle(k, 1)

    assert k.batches == [["kernel", "readings", "totals", "runner"]], (
        "all four calls must dispatch in ONE kernel batch"
    )
    assert len(k.sent) == 1, "one destination -> exactly one delivered message"
    target, payload = k.sent[0]
    assert target == "ai"
    blocks = payload["text"].split("\n\n")
    assert len(blocks) == 4
    for n, (block, agent) in enumerate(
        zip(blocks, ["kernel", "readings", "totals", "runner"]), 1
    ):
        assert block.startswith(f"[reply to {agent}:pid · {agent} reflect · {n} of 4")


async def test_two_destinations_get_two_messages_with_their_own_counts():
    """Per-call `reply_to` splits the batch: each destination's M counts only
    what IT receives; both messages share the turn's batch token."""
    k = _Kernel()
    text = _call("kernel") + _call("totals", reply_to="scribe") + _call("readings")
    await _route_reply("ai", k, text, "cli", None, None, 180.0, "")
    await _settle(k, 2)

    by_dest = {t: p["text"] for t, p in k.sent}
    assert set(by_dest) == {"ai", "scribe"}
    assert " · 1 of 2 in batch " in by_dest["ai"]
    assert " · 2 of 2 in batch " in by_dest["ai"]
    assert "in batch" not in by_dest["scribe"], "a lone block carries no suffix"
    token = by_dest["ai"].split("in batch ")[1][:4]
    assert token  # the same turn token names the round on the multi-block side


async def test_fire_and_forget_still_dispatches_but_is_not_counted():
    """`reply_to:null` runs the call and discards the answer — it appears in
    the kernel batch and in NO delivered message, and M ignores it."""
    k = _Kernel()
    text = _call("kernel") + _call("totals", reply_to="null")
    await _route_reply("ai", k, text, "cli", None, None, 180.0, "")
    await _settle(k, 1)

    assert k.batches == [["kernel", "totals"]], "the discarded call still ran"
    assert len(k.sent) == 1
    body = k.sent[0][1]["text"]
    assert "totals" not in body
    assert "in batch" not in body, "one awaited call -> no batch suffix"


async def test_the_layer_default_timeout_rides_every_item():
    """The AI layer ALWAYS supplies a per-call deadline (the kernel defaults
    nothing), and the model's own `timeout` arg overrides it for that call."""
    seen: list[tuple[str, object]] = []

    class _K(_Kernel):
        async def send(self, items):
            for it in items:
                if it["payload"].get("type") != "send":
                    seen.append((it["target"], it.get("timeout")))
            return await super().send(items)

    k = _K()
    with_own = (
        "<tool_call>"
        + json.dumps(
            {
                "name": "send",
                "arguments": {
                    "target_id": "slowpoke",
                    "payload": {"type": "crunch"},
                    "reply_to": "self",
                    "timeout": 5,
                },
            }
        )
        + "</tool_call>"
    )
    await _route_reply("ai", k, _call("kernel") + with_own, "cli", None, None, 42.0, "")
    await _settle(k, 1)
    assert seen == [("kernel", 42.0), ("slowpoke", 5)]


async def test_the_models_execution_id_rides_the_item(monkeypatch=None):
    """`id` in the envelope names the execution at the KERNEL — the model's
    handle on the ledger (dedup, join, recorded outcomes). Passed through
    verbatim; the kernel validates."""
    seen: list[tuple[str, object]] = []

    class _K(_Kernel):
        async def send(self, items):
            for it in items:
                if it["payload"].get("type") != "send":
                    seen.append((it["target"], it.get("id"), it.get("retries")))
            return await super().send(items)

    k = _K()
    named = (
        "<tool_call>"
        + json.dumps(
            {
                "name": "send",
                "arguments": {
                    "target_id": "vault",
                    "payload": {"type": "withdraw"},
                    "reply_to": "self",
                    "id": "job-alpha",
                    "retries": 2,
                },
            }
        )
        + "</tool_call>"
    )
    await _route_reply("ai", k, _call("kernel") + named, "cli", None, None, 42.0, "")
    await _settle(k, 1)
    assert seen == [("kernel", None, None), ("vault", "job-alpha", 2)]


async def test_a_failed_outcome_is_rendered_not_hidden():
    """A call that failed or timed out occupies its slot in M with its error
    body — a partial batch is the one thing the model must never see."""

    class _K(_Kernel):
        async def send(self, items):
            outs = []
            for it in items:
                if it["payload"].get("type") == "send":
                    self.sent.append((it["target"], it["payload"]))
                    outs.append({"pid": None, "status": "completed", "result": None})
                elif it["target"] == "hung":
                    outs.append(
                        {
                            "pid": "hung:1",
                            "status": "timed_out",
                            "result": {
                                "error": "no reply after 42s",
                                "reason": "timeout",
                            },
                        }
                    )
                else:
                    outs.append(
                        {
                            "pid": f"{it['target']}:pid",
                            "status": "completed",
                            "result": {"ok": True},
                        }
                    )
            return outs

    k = _K()
    await _route_reply(
        "ai", k, _call("hung") + _call("kernel"), "cli", None, None, 42.0, ""
    )
    await _settle(k, 1)
    body = k.sent[0][1]["text"]
    blocks = body.split("\n\n")
    assert len(blocks) == 2
    assert blocks[0].startswith("[reply to hung:1 · hung reflect · 1 of 2")
    assert "no reply after 42s" in blocks[0]
    assert blocks[1].startswith("[reply to kernel:pid · kernel reflect · 2 of 2")
