"""C1 — resume after suspension continues the run (the AI-chain variant).

The exemplar (`openai-agents/tests/test_hitl_session_scenario.py`) interrupts a
run for human input and asserts the answer resumes it at the exact point. In
this paradigm suspension needs NO machinery: a turn that asks and emits no call
ENDS the chain; durable history holds the question; the human's answer arrives
as the next `send`, and that turn resumes with full context. The assertions
that make it C1 rather than "just chat":

  1. work done BEFORE the ask happened exactly once and is NOT re-run on resume
     (the ledger's dedup is what makes this assertable, not just observable);
  2. the resumed turn CONTINUES — the post-answer side effect lands after it;
  3. the structured question shape (`{type:'inquiry', request_id, kind, ...}`)
     survives to the asker verbatim, so a UI can render it and correlate.

The script-mind variant of C1 was already pinned in `test_mind_plumbed` (the
ASK flow); the registry row points at both.
"""

from __future__ import annotations

import asyncio
import json

import pytest

from ai_core import core

QUESTION = {
    "type": "inquiry",
    "request_id": "q-color",
    "kind": "single",
    "question": "Which label do I write?",
    "options": ["red", "green"],
}


def _call(verb_payload: dict) -> str:
    return (
        "<tool_call>"
        + json.dumps(
            {
                "name": "send",
                "arguments": {
                    "target_id": "c1_store",
                    "payload": verb_payload,
                    "reply_to": "self",
                    "id": f"c1-{verb_payload['path']}",
                },
            }
        )
        + "</tool_call>"
    )


class _FakeProvider:
    """Scripted mind: does one write, asks, and — only when the ANSWER turn
    arrives — does the dependent write and finishes."""

    def __init__(self):
        self.calls = 0
        self.requests: list = []

    async def chat(self, messages):
        self.calls += 1
        self.requests.append(messages)
        last = messages[-1]["content"]
        if self.calls == 1:
            # step 1: a side effect, then next turn will hold its reply
            yield _call({"type": "write", "path": "step1", "bytes": "done"})
        elif "green" in last and "[reply to" not in last:
            # THE RESUME TURN: the human's answer — continue exactly here
            yield _call({"type": "write", "path": "step2", "bytes": "green-label"})
        elif "[reply to" in last:
            if "step2" in last:
                yield "FINISHED."
            else:
                # step 1's reply came back → now we need the human: ask + stop
                yield "I need a decision.\n" + json.dumps(QUESTION) + "\n"
        else:
            yield "unexpected turn"


@pytest.fixture
async def wired(seeded_kernel, store_agent):
    seeded_kernel.create(
        "text_file.tools",
        id="c1_store",
        target_id=store_agent,
        can_call={store_agent: "*"},
        callable_by={"c1_ai": "*"},
    )
    # the surface must ACCEPT the editor (two-sided gate)
    await seeded_kernel.send(
        [
            {
                "target": seeded_kernel.id,
                "payload": {
                    "type": "update_agent",
                    "id": store_agent,
                    "callable_by": {"c1_store": "*"},
                },
            }
        ]
    )
    seeded_kernel.create(
        "ollama_backend.tools",
        id="c1_ai",
        context_window=32768,
        # The chat store inherits ITS OWNER'S leg — without one, history has
        # nowhere durable to land and the recorded ask this test reads back
        # never persists.
        file_bridge_id=store_agent,
        can_call={"cli": "*", "c1_store": "*"},
    )
    fp = _FakeProvider()
    core._providers["c1_ai"] = fp
    try:
        yield seeded_kernel, fp
    finally:
        core._providers.pop("c1_ai", None)


async def _value(kernel, key):
    r = (
        await kernel.send(
            [{"target": "c1_store", "payload": {"type": "read", "path": key}}]
        )
    )[0]["result"]
    if not isinstance(r, dict) or "lines" not in r:
        return None  # not_found (or refused) — the file was never written
    return "\n".join(ln["text"] for ln in r["lines"]) or None


async def _settle(fp, want_calls, timeout=5.0):
    t0 = asyncio.get_event_loop().time()
    while fp.calls < want_calls:
        assert asyncio.get_event_loop().time() - t0 < timeout, (
            f"chain stalled at {fp.calls} passes"
        )
        await asyncio.sleep(0.02)
    await asyncio.sleep(0.1)  # let routing drain


async def test_a_chain_suspends_on_a_question_and_the_answer_resumes_it(wired):
    kernel, fp = wired

    # ── phase 1: run until the ask ──
    first = (
        await kernel.send(
            [
                {
                    "target": "c1_ai",
                    "payload": {"type": "send", "text": "do the two-step task"},
                }
            ]
        )
    )[0]["result"]
    await _settle(fp, 2)
    # SUSPENDED: the ask turn emitted no call, the chain is over, step1 done
    assert await _value(kernel, "step1") == "done"
    assert await _value(kernel, "step2") is None, "step2 must WAIT for the human"

    # the structured question reached the asker verbatim (renderable + correlatable)
    assert first is not None
    hist = (
        await kernel.send(
            [{"target": "c1_ai", "payload": {"type": "recall", "limit": 100}}]
        )
    )[0]["result"]["messages"]
    ask_texts = [m["content"] for m in hist if m["role"] == "assistant"]
    ask = next((t for t in ask_texts if "request_id" in t), None)
    assert ask, f"no structured question in: {ask_texts}"
    q = json.loads(ask[ask.index("{") : ask.rindex("}") + 1])
    assert q == QUESTION

    calls_at_suspend = fp.calls

    # ── phase 2: the human answers — a plain next send, nothing special ──
    (
        await kernel.send(
            [
                {
                    "target": "c1_ai",
                    "payload": {"type": "send", "text": "q-color: green"},
                }
            ]
        )
    )[0]["result"]
    await _settle(fp, calls_at_suspend + 2)

    # ── the C1 claims ──
    assert await _value(kernel, "step2") == "green-label", "resume did not continue"
    # completed pre-ask work ran exactly ONCE: the named execution's ledger row
    led = (await kernel.send([{"target": "kernel", "payload": {"type": "ledger"}}]))[0][
        "result"
    ]
    rows = {r.get("name"): r for r in led["executions"] if r.get("name")}
    assert rows["c1-step1"]["attempts"] == 1, "suspension re-ran completed work"
    assert rows["c1-step2"]["status"] == "completed"
    # and the resume turn saw its full pre-suspension history
    resume_msgs = fp.requests[calls_at_suspend]
    joined = json.dumps(resume_msgs)
    assert "step1" in joined and "request_id" in joined, (
        "the resumed turn lost its pre-suspension context"
    )
