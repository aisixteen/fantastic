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

from ai_core.tool_parse import render_call
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
    return render_call("send", args)


class _Kernel:
    """Records what would be delivered back to the model."""

    def __init__(self):
        self.delivered: list[str] = []

    def get(self, _id):
        return {}

    def spawn_task(self, _agent_id, _verb, coro):
        """Deliveries are FORKED through the kernel (ai_core has one path, no
        unregistered fallback) — run the work as a task, like the kernel does."""
        self._tasks = getattr(self, "_tasks", [])
        self._tasks.append(asyncio.get_running_loop().create_task(coro))
        return len(self._tasks)

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
# ONLY THE ANSWER ACTS (owner, 2026-10-07): a call written inside thinking is
# thinking — not dispatched and not stored; dispatch and history both read the
# answer, so they can never disagree.

from ai_core.core import without_thinking  # noqa: E402

_C = render_call("send", {"target_id": "totals", "payload": {"type": "read"}})


def _pair(text: str) -> tuple[int, int]:
    """(dispatched, persisted) — both read the ANSWER, so they never disagree."""
    from ai_core.tool_parse import extract_tool_calls

    answer = without_thinking(text)
    return len(extract_tool_calls(answer)), len(extract_tool_calls(answer))


def test_a_call_inside_a_thought_is_neither_dispatched_nor_stored():
    assert _pair(f"<think>let me check {_C} done</think>") == (0, 0)


def test_a_thought_is_dropped_whole_calls_included():
    kept = without_thinking(f"<think>long private reasoning {_C} more</think>")
    assert kept == ""


def test_an_unclosed_thought_runs_to_the_end():
    """An unclosed `<think>` is thinking to the end — a call in it does not act."""
    assert _pair(f"<think>cut mid {_C}") == (0, 0)
    assert without_thinking(f"visible<think>cut mid {_C}") == "visible"


def test_a_thought_with_no_call_persists_as_nothing():
    assert without_thinking("<think>reasoning, no answer</think>") == ""


def test_only_the_answers_call_counts():
    assert _pair(f"<think>a {_C} b</think>{_C}") == (1, 1)


# ─── docs replies: transient, and pointed at from refusals ───────


def test_a_docs_reply_is_cut_to_a_stub_once_answered():
    from ai_core.core import _DOCS_DROPPED, drop_read_docs

    body = (
        "[reply to 12 · kernel docs]\n<of>runner</of>\n<bundle>exec_bridge.tools</bundle>"
        "\n<tier>oneliners</tier>\n<verbs>\n<start>args: command:str</start>\n</verbs>"
    )
    sibling = "[reply to 13 · notes read]\nline one"
    hist = [
        {"role": "user", "content": body + "\n\n" + sibling, "id": 7},
        {"role": "assistant", "content": "<call …>", "id": 8},
    ]
    out = drop_read_docs(hist)
    blocks = out[0]["content"].split("\n\n")
    assert blocks[0] == "[reply to 12 · kernel docs]\n" + _DOCS_DROPPED.format(
        of="runner", tier="oneliners", pid="12", verb="start"
    )
    # the grep example names a REAL verb of the body, never a placeholder
    assert "<ids><item>12</item></ids> <query>start</query>" in blocks[0]
    assert blocks[1] == sibling and out[0]["id"] == 7
    # not answered yet → whole
    assert drop_read_docs(hist[:1]) == hist[:1]
    # a refusal (no `of`) stays whole
    refused = [
        {"role": "user", "content": "[reply to 9 · kernel docs]\n<error>x</error>"}
    ]
    assert drop_read_docs(refused + hist[1:])[0] == refused[0]


async def test_a_docs_body_is_held_and_its_first_line_names_the_grep(
    seeded_kernel, fake_providers
):
    """A docs body opens with the line saying it is erased after the next message
    and how to keep what is needed (owner, 2026-10-06); the body is HELD under its
    pid, so the model's own `grep` on that pid returns the verb line."""
    from _testkit import memory_meta

    k = seeded_kernel

    class _Mind:
        def __init__(self):
            self.incoming: list[str] = []

        async def chat(self, messages, max_tokens=None):
            last = messages[-1]["content"]
            self.incoming.append(last)
            if len(self.incoming) == 1:
                yield render_call(
                    "send",
                    {
                        "target_id": "kernel",
                        "payload": {"type": "docs", "of": "kernel"},
                    },
                )
            elif len(self.incoming) == 2:
                pid = re.search(r"<ids><item>(\d+)</item></ids>", last).group(1)
                yield render_call(
                    "send",
                    {
                        "target_id": "rm_docs",
                        "payload": {
                            "type": "grep",
                            "query": "ack_deadletter",
                            "ids": [int(pid)],
                        },
                    },
                )
            else:
                yield '<call name="done"></call>'

    mind = _Mind()
    k.create(
        "ollama_backend.tools",
        id="rm_docs",
        context_window=32768,
        **memory_meta(k, "rm_docs", {"kernel_state": ["docs"], "cli": "*"}),
    )
    root = k.ctx.agents["kernel_state"]
    root._meta["callable_by"] = {
        **(root._meta.get("callable_by") or {}),
        "rm_docs": ["docs"],
    }
    fake_providers["rm_docs"] = mind
    try:
        await k.send([{"target": "rm_docs", "payload": {"type": "send", "text": "go"}}])
        for _ in range(250):
            if len(mind.incoming) >= 3:
                break
            await asyncio.sleep(0.02)
        docs_turn, grep_turn = mind.incoming[1], mind.incoming[2]
        head = docs_turn.split("\n")[1]
        assert head.startswith("[erased from your view after your next message"), (
            docs_turn
        )
        assert "<query>create_agent</query>" in head, (
            head
        )  # the first kernel verb, not a placeholder
        assert "<of>kernel</of>" in docs_turn
        assert "rm_docs grep" in grep_turn and "<line>" in grep_turn, grep_turn
        assert "ack_deadletter" in grep_turn.split("rm_docs grep", 1)[1], grep_turn
    finally:
        fake_providers.pop("rm_docs", None)


def test_the_grep_example_is_a_real_verb_and_a_verbless_body_is_not_cut():
    from ai_core.core import drop_read_docs, example_verb

    assert (
        example_verb(
            "<of>x</of>\n<verbs>\n<reflect>a</reflect>\n<boot>b</boot>\n<write>c\nmore</write>\n</verbs>"
        )
        == "write"
    )
    assert (
        example_verb("<of>x</of>\n<verbs>\n<reflect>a</reflect>\n</verbs>") == "reflect"
    )
    assert example_verb("<of>x</of>\n<verbs>\n</verbs>") is None
    bare = "[reply to 5 · kernel docs]\n<of>x</of>\n<bundle>x.tools</bundle>\n<tier>oneliners</tier>\n<verbs>\n</verbs>"
    hist = [
        {"role": "user", "content": bare},
        {"role": "assistant", "content": "<call …>"},
    ]
    assert drop_read_docs(hist) == hist  # nothing held, so nothing erased


def test_a_refusal_of_the_models_call_points_at_docs():
    from ai_core.core import point_to_docs

    unk = {"error": "x: unknown type 'zz'", "reason": "unhandled", "verb": "zz"}
    assert "docs, of: 'fs'" in point_to_docs("fs", "zz", unk)["hint"]
    miss = {"error": "read: path required", "reason": "missing_arg", "arg": "path"}
    assert "verb: 'read', tier: detailed" in point_to_docs("fs", "read", miss)["hint"]
    # a hint of its own wins; a bad_arg, a relayed refusal and an arg-less one stay
    own = {**miss, "hint": "mine"}
    assert point_to_docs("fs", "read", own) == own
    bad = {"error": "e", "reason": "bad_arg", "arg": "path"}
    assert point_to_docs("fs", "read", bad) == bad
    relayed = {**unk, "verb": "other"}
    assert point_to_docs("leg", "forward", relayed) == relayed
    nameless = {"error": "e", "reason": "missing_arg"}
    assert point_to_docs("ai", "send", nameless) == nameless
