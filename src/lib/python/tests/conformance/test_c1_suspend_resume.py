"""C1 — resume after asking continues the run (the AI-chain variant).

The exemplar (`openai-agents/tests/test_hitl_session_scenario.py`) interrupts a
run for human input and asserts the answer resumes it at the exact point. In
this paradigm there is NO SUSPENSION MACHINERY, and since 2026-09-11 there is
no suspension either: ASKING IS A CALL. The chain `send`s an `inquiry` to the
`user` agent and the human's answer comes back as THAT CALL'S REPLY, in the
next message, tagged and correlated like any other. Nothing is scraped out of
prose and nothing waits on a convention.

What made the old shape untenable: the mind asked by printing a JSON blob and
emitting no call, and a call-less message ENDED the chain. A message that emits no
call is now `try again`, so a printed question is not an ending — it is a model
that forgot to act. The three claims that make this C1 rather than "just chat"
survive the change unaltered:

  1. work done BEFORE the ask happened exactly once and is NOT re-run after the
     answer (the ledger's dedup is what makes this assertable, not just
     observable);
  2. the chain CONTINUES — the post-answer side effect lands after it;
  3. the structured question reaches the HUMAN verbatim (`request_id`, `kind`,
     `options`), read off the desk's own record of the call it received, so a
     UI can render it and correlate the answer.
"""

from __future__ import annotations

import asyncio
import json
import sys
import types

import pytest

from ai_core import core

QUESTION = {
    "request_id": "q-color",
    "kind": "single",
    "text": "Which label do I write?",
    "options": ["red", "green"],
}

#: What the desk was actually handed, per agent id — the SCORE. A model that
#: only talks about asking leaves this empty.
_ASKED: dict[str, list] = {}


async def _user_handler(id, payload, kernel):
    """The human, as an ordinary agent: the answer IS the reply."""
    verb = payload.get("type")
    if verb in ("boot", "shutdown"):
        return None
    if verb == "reflect":
        return {"id": id, "sentence": "the human", "verbs": {"inquiry": "ask me"}}
    if verb == "inquiry":
        _ASKED.setdefault(id, []).append(
            {k: v for k, v in payload.items() if k != "type"}
        )
        return {"answer": "green", "request_id": payload.get("request_id")}
    if verb == "asked":
        return {"asked": _ASKED.get(id, [])}
    return {"error": f"user: unknown verb {verb!r}", "reason": "unhandled"}


_user_mod = types.ModuleType("c1_user.tools")
_user_mod.handler = _user_handler
sys.modules["c1_user.tools"] = _user_mod


def _call(verb_payload: dict) -> str:
    from ai_core.tool_parse import render_call

    return render_call(
        "send",
        {
            "target_id": "c1_store",
            "payload": verb_payload,
            "reply_to": "self",
            "exec_id": f"c1-{verb_payload['path']}",
        },
    )


def _ask() -> str:
    """The ask, as a CALL to the human. Its reply is the answer."""
    from ai_core.tool_parse import render_call

    return render_call(
        "send",
        {"target_id": "c1_user", "payload": {"type": "inquiry", **QUESTION}},
    )


class _FakeProvider:
    """Scripted mind: does one write, asks, and — only when the ANSWER message
    arrives — does the dependent write and finishes."""

    def __init__(self):
        self.calls = 0
        self.requests: list = []

    async def chat(self, messages):
        self.calls += 1
        self.requests.append(messages)
        last = messages[-1]["content"]
        if self.calls == 1:
            # step 1: a side effect, then next message will hold its reply
            yield _call({"type": "write", "path": "step1", "bytes": "done"})
        elif "[reply to" in last:
            if "step2" in last:
                yield "FINISHED. " + '<call name="done"></call>'
            elif "green" in last:
                # THE ANSWER CAME BACK AS THE REPLY — continue exactly here.
                yield _call({"type": "write", "path": "step2", "bytes": "green-label"})
            else:
                # step 1's reply came back → now we need the human. ASKING IS A
                # CALL: the chain does not end here, it waits on this reply.
                yield "I need a decision. " + _ask()
        else:
            yield "unexpected message " + '<call name="done"></call>'


@pytest.fixture
async def wired(seeded_kernel, store_agent):
    _ASKED.clear()
    seeded_kernel.create(
        "c1_user.tools",
        id="c1_user",
        callable_by={"c1_ai": "*"},
    )
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
        can_call={"cli": "*", "c1_store": "*", "c1_user": "*"},
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


async def test_a_chain_asks_the_human_and_the_answer_resumes_it(wired):
    kernel, fp = wired

    # ONE send. The chain does step 1, asks the human, reads the answer as that
    # call's reply, does step 2 and finishes — no second `send` from outside,
    # because nothing suspended.
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
    assert first is not None
    await _settle(fp, 4)

    # CLAIM 3 — the structured question reached the HUMAN verbatim, read off the
    # desk's own record of the call it received, not out of the model's prose.
    asked = (await kernel.send([{"target": "c1_user", "payload": {"type": "asked"}}]))[
        0
    ]["result"]["asked"]
    assert len(asked) == 1, f"the human was asked {len(asked)} times: {asked}"
    assert {k: asked[0][k] for k in QUESTION} == QUESTION, asked[0]

    # CLAIM 2 — the chain CONTINUED past the answer.
    assert await _value(kernel, "step1") == "done"
    assert await _value(kernel, "step2") == "green-label", (
        "the answer did not resume it"
    )
    # completed pre-ask work ran exactly ONCE: the named execution's ledger row
    led = (await kernel.send([{"target": "kernel", "payload": {"type": "ledger"}}]))[0][
        "result"
    ]
    rows = {r.get("name"): r for r in led["executions"] if r.get("name")}
    assert rows["c1-step1"]["attempts"] == 1, "suspension re-ran completed work"
    assert rows["c1-step2"]["status"] == "completed"
    # and the message that continued saw its full pre-ask history
    joined = json.dumps(fp.requests[-1])
    assert "step1" in joined and "request_id" in joined, (
        "the continuing message lost its pre-ask context"
    )
