"""The `N of M from that turn` stamp on a reply envelope.

The failure it exists to prevent, measured three times and fatal each time: a turn
emits several calls, each reply comes back as its own turn, and the model cannot
tell "the rest are still in flight" from "the kernel answered the wrong one".
2026-08-07 opus re-issued both deletes and theorised duplicate delivery;
2026-08-20 the model decided the reply queue was running ahead of it and re-polled
forever; 2026-08-24 the model made two parallel reflects, got the first back, wrote
"I sent a reflect to file_bridge but the reply came back from exec_bridge",
re-sent it and gave up without doing the task.

Since 2026-08-25 the whole batch arrives as ONE message per destination, so
the stamp's remaining jobs are correlation (each block to its dispatch pid) and
completeness (the model can count M blocks and reconcile). The tag format is
unchanged.
"""

from __future__ import annotations

import asyncio
import json
import re

from ai_core.core import _REPLY_TAG, _route_reply


async def _settle(k, want: int, timeout: float = 2.0) -> None:
    """Wait for the spawned deliveries.

    `_route_reply` no longer AWAITS the turns its replies start — that is the fix
    for the 26-of-36 reply loss measured 2026-08-24. A caller that wants to observe
    a delivery therefore yields to the loop first."""
    began = asyncio.get_event_loop().time()
    while len(k.delivered) < want:
        if asyncio.get_event_loop().time() - began > timeout:
            break
        await asyncio.sleep(0.005)


def _call(target: str, verb: str, reply_to: str | None = "self") -> str:
    args = {"target_id": target, "payload": {"type": verb}}
    if reply_to is not None:
        args["reply_to"] = reply_to
    return (
        "<tool_call>" + json.dumps({"name": "send", "arguments": args}) + "</tool_call>"
    )


class _Kernel:
    """Records what would be delivered back to the model."""

    def __init__(self):
        self.delivered: list[str] = []

    def get(self, _id):
        return {}

    async def send(self, items):
        outs = []
        for it in items:
            target, payload = it["target"], it["payload"]
            if payload.get("type") == "send":
                self.delivered.append(payload["text"])
                outs.append({"pid": None, "status": "completed", "result": {}})
            else:
                outs.append(
                    {
                        "pid": f"{target}:beef",
                        "status": "completed",
                        "result": {"ok": target},
                    }
                )
        return outs


async def test_a_single_call_is_not_stamped():
    """One call needs no bookkeeping; the tag stays exactly as it was."""
    k = _Kernel()
    await _route_reply(
        "ai", k, _call("kernel_state", "reflect"), "probe", None, None, 180.0, ""
    )
    await _settle(k, 1)
    assert len(k.delivered) == 1
    assert "in batch" not in k.delivered[0]
    assert k.delivered[0].startswith(
        "[reply to kernel_state:beef · kernel_state reflect]"
    )


async def test_parallel_calls_each_say_how_many_are_coming():
    """Both blocks arrive together, stamped 1 of 2 / 2 of 2, one token."""
    k = _Kernel()
    text = _call("exec_bridge", "reflect") + _call("file_bridge", "reflect")
    await _route_reply("ai", k, text, "probe", None, None, 180.0, "")
    await _settle(k, 1)
    assert len(k.delivered) == 1, "one destination -> ONE message, blocks inside"
    blocks = k.delivered[0].split("\n\n")
    assert re.search(r"· 1 of 2 in batch [0-9a-f]{4}\]", blocks[0]), blocks[0]
    assert re.search(r"· 2 of 2 in batch [0-9a-f]{4}\]", blocks[1]), blocks[1]
    # …and BOTH carry the SAME token: that is what makes two rounds distinguishable.
    toks = [re.search(r"in batch ([0-9a-f]{4})", b).group(1) for b in blocks]
    assert toks[0] == toks[1], f"one turn must be one batch: {toks}"


async def test_fire_and_forget_calls_are_not_counted():
    """`reply_to:null` produces no reply, so it must not inflate the total —
    a model told "1 of 2" while only one is coming would wait forever."""
    k = _Kernel()
    text = _call("a", "reflect") + _call("b", "set", reply_to="null")
    await _route_reply("ai", k, text, "probe", None, None, 180.0, "")
    await _settle(k, 1)
    assert len(k.delivered) == 1
    assert "in batch" not in k.delivered[0]


async def test_the_tag_regex_still_parses_a_stamped_envelope():
    """`_REPLY_TAG` drives stale-read dedup; the suffix must not break it."""
    m = _REPLY_TAG.match(
        "[reply to ai:beef · kernel_state reflect · 1 of 3 in batch a1b2]"
    )
    assert m and m.group(1) == "kernel_state" and m.group(2) == "reflect"
    m2 = _REPLY_TAG.match("[reply to ai:beef · kernel_state reflect]")
    assert m2 and m2.group(1) == "kernel_state" and m2.group(2) == "reflect"


# ── a call written inside a thought ──────────────────────────────────────────
# Dispatch reads the FULL text; history read the STRIPPED text. The two disagreed,
# so a call made from inside a thought ran, returned a reply, and left no record —
# the model saw an effect with no cause. Measured 2026-08-24 (`live_compose`).

from ai_core.core import without_thinking  # noqa: E402

_C = (
    '<tool_call>{"name":"send","arguments":'
    '{"target_id":"totals","payload":{"type":"read"}}}</tool_call>'
)


def _pair(text: str) -> tuple[int, int]:
    """(dispatched, persisted) — these must never disagree."""
    from ai_core.tool_parse import extract_tool_calls

    return len(extract_tool_calls(text)), len(
        extract_tool_calls(without_thinking(text))
    )


def test_a_call_inside_a_thought_survives_into_history():
    """The exact shape that broke live_compose: the call ran, the reply came back,
    and the model's own history said it had never asked."""
    assert _pair(f"<think>let me check {_C} done</think>") == (1, 1)


def test_reasoning_around_a_kept_call_is_still_dropped():
    """The action stays, the deliberation goes — feeding a reasoning model its own
    prior thought is the compounding failure this stripper exists to prevent."""
    kept = without_thinking(f"<think>long private reasoning {_C} more</think>")
    assert "long private reasoning" not in kept
    assert "more" not in kept
    assert _C in kept


def test_an_unterminated_thought_still_keeps_its_call():
    """A stream cut mid-thought drops the reasoning tail, but a call already
    dispatched from it is an action that happened."""
    assert _pair(f"<think>cut mid {_C}") == (1, 1)


def test_a_thought_with_no_call_persists_as_nothing():
    """Unchanged: pure deliberation leaves no trace in durable history."""
    assert without_thinking("<think>reasoning, no answer</think>") == ""


def test_calls_inside_and_outside_one_turn_both_persist():
    assert _pair(f"<think>a {_C} b</think>{_C}") == (2, 2)


# ── the OTHER two dialects, inside a thought ─────────────────────────────────
# The stripper kept only `<tool_call>` spans while DISPATCH parsed all three, so
# the identical effect-without-cause gap stayed wide open for any model that
# speaks another grammar — gemma-4 writes the CHANNEL form natively. Rust's
# `kept_envelopes` has scanned all three since it was written.

_CHANNEL = "<|tool_call>call:kernel_state.list_agents{}<tool_call|>"
_INVOKE = (
    '<invoke name="send"><parameter name="arguments">'
    '{"target_id":"notes","payload":{"type":"read"}}'
    "</parameter></invoke>"
)


def test_a_channel_call_and_an_invoke_inside_a_thought_survive_into_history():
    """Both non-JSON dialects are kept, IN SOURCE ORDER, and the reasoning around
    them still goes. Order matters: the record must state what happened in the
    sequence it happened, not grouped by which grammar was used."""
    text = (
        f"<think>first I list the tree {_CHANNEL} then I read the note "
        f"{_INVOKE} secret deliberation</think>"
    )
    assert _pair(text) == (2, 2)
    kept = without_thinking(text)
    assert _CHANNEL in kept and _INVOKE in kept
    assert kept.index(_CHANNEL) < kept.index(_INVOKE), "source order, not table order"
    for prose in ("first I list the tree", "then I read the note", "secret"):
        assert prose not in kept, prose


def test_all_three_dialects_mixed_in_one_thought_are_all_kept():
    """A turn may mix grammars; none of the three is privileged."""
    text = f"<think>a {_INVOKE} b {_C} c {_CHANNEL} d</think>"
    assert _pair(text) == (3, 3)
    kept = without_thinking(text)
    assert kept.index(_INVOKE) < kept.index(_C) < kept.index(_CHANNEL)


def test_an_unterminated_thought_keeps_the_other_dialects_too():
    """The unterminated branch is the same rule — it used to have its own,
    `<tool_call>`-only, copy of the scan."""
    assert _pair(f"<think>cut mid {_CHANNEL}") == (1, 1)
    assert _pair(f"<think>cut mid {_INVOKE}") == (1, 1)


def test_an_unclosed_envelope_in_a_thought_is_reasoning_not_an_action():
    """Only COMPLETE spans are kept: dispatch could not parse an opener with no
    closer either, so keeping it would record an action that never ran."""
    assert without_thinking("<think>maybe <|tool_call>call:x.y{} …</think>") == ""
