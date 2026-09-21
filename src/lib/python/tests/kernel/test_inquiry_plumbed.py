"""INQUIRY, END TO END, SCORED ON THE HUMAN'S OWN TALLY.

An AI agent cannot proceed without a decision that is not its to make, so it
`send`s an `inquiry` to the `user` agent — the human, first-class, with an id, a
record, a gate and a ledger row like anyone else. The answer comes back as that
call's REPLY in the model's next message. Nothing suspends and nothing is scraped.

WHAT THIS FILE REPLACES, and why. `test_mind_plumbed.py` drove a backend as a
PROSE ORACLE: an authored program sent text, read the completion's `response`
as data, and parsed `CALL x y` / `FINAL: …` out of it. Two things ended that.
Tool calls became mandatory — a message that emits no `<call>` is told to try
again, so a prose reply is never an ending — and the inquiry stopped being a
text convention. The old file asserted on the SHAPE OF PROSE, which is exactly
what a model can produce without doing anything.

So the score here is the human's own count. `user.inquiries` is the number of
inquiries that agent ACTUALLY RECEIVED, incremented inside its handler; a message
that merely talks about asking leaves it at zero. Same for `proposals`: an
`action` a sender may not perform itself is HELD by the human, read back off
the call it really received. Mirrors `e2e/probes/user_probe` (the live twin) so
the wiring a unit exercises is the wiring an e2e run exercises.
"""

from __future__ import annotations

import asyncio
import sys
import types

import pytest

import ollama_backend.tools as ot

# ─── the `user` agent: the human as an ordinary bundle ──────────
#
# Built as a module object in `sys.modules`, which IS a bundle as far as the
# substrate is concerned (`Agent._dispatch` resolves handlers with
# `importlib.import_module`). Same idiom as `test_task_table`'s slow bundle.

#: agent id → inquiries this agent really received. THE SCORE.
_RECEIVED: dict[str, int] = {}
#: agent id → every `action` proposal handed to it, in arrival order.
_PROPOSALS: dict[str, list] = {}

KINDS = ("text", "single", "multi", "confirm")


@pytest.fixture(autouse=True)
def _fresh_desk():
    """The tally is MODULE state and every test wires a `user` at the same id,
    so it accumulates across tests in one worker — the second test would open
    with the first's count and score a pass it never earned. Each test gets an
    empty desk."""
    _RECEIVED.clear()
    _PROPOSALS.clear()
    yield
    _RECEIVED.clear()
    _PROPOSALS.clear()


USER_SENTENCE = (
    "The human. Send me an inquiry when you cannot proceed without a decision "
    "that is mine to make; the answer comes straight back as the reply."
)


async def _user_handler(id, payload, kernel):
    verb = payload.get("type")
    if verb in ("boot", "shutdown"):
        return None
    if verb == "reflect":
        return {
            "id": id,
            "sentence": USER_SENTENCE,
            "verbs": {
                "inquiry": "args: text:str (req), kind:str?, options:list?, "
                "action:dict? — RETURNS {answer, request_id}.",
                "inquiries": "no args — {inquiries, proposals}: the tally.",
            },
            "inquiries": _RECEIVED.get(id, 0),
        }
    if verb == "inquiry":
        text = payload.get("text")
        if not isinstance(text, str) or not text.strip():
            return {
                "error": "user: `text` must be a non-empty string",
                "reason": "bad_arg",
                "arg": "text",
            }
        kind = payload.get("kind", "text")
        if kind not in KINDS:
            return {
                "error": f"user: `kind` must be one of {list(KINDS)}",
                "reason": "bad_arg",
                "arg": "kind",
            }
        answer = (kernel.get(id) or {}).get("answer")
        # NO DEFAULT. A human's words are never invented: an unwired `user`
        # says so rather than fabricating consent.
        if not isinstance(answer, str) or not answer:
            return {
                "error": "user: no `answer` on this agent's record — the "
                "operator wires what the human says",
                "reason": "not_connected",
            }
        action = payload.get("action")
        if isinstance(action, dict):
            _PROPOSALS.setdefault(id, []).append(action)
        _RECEIVED[id] = _RECEIVED.get(id, 0) + 1
        return {
            "answer": answer,
            "request_id": payload.get("request_id"),
            "inquiries": _RECEIVED[id],
        }
    if verb == "inquiries":
        return {
            "inquiries": _RECEIVED.get(id, 0),
            "proposals": _PROPOSALS.get(id, []),
        }
    return {"error": f"user: unknown verb {verb!r}", "reason": "unhandled"}


_user_mod = types.ModuleType("user_agent.tools")
_user_mod.handler = _user_handler
sys.modules["user_agent.tools"] = _user_mod


# ─── a scripted model ───────────────────────────────────────────


class _Script:
    """One scripted completion per `chat()` call. `requests` keeps every pass's
    messages, so a test can prove the human's answer became the NEXT message."""

    def __init__(self, messages: list[str]):
        self._messages = list(messages)
        self.requests: list = []
        self.calls = 0

    async def chat(self, messages):
        self.requests.append(messages)
        self.calls += 1
        if self._messages:
            yield self._messages.pop(0)

    def incoming(self, n: int) -> str:
        """The user-role text the model READ on pass `n` (0-based)."""
        msgs = self.requests[n]
        return next(
            (m.get("content", "") for m in reversed(msgs) if m.get("role") == "user"),
            "",
        )


def _ask(text: str, *, kind: str = "text", **extra) -> str:
    """An `inquiry` call in the ONE grammar — one child tag per argument."""
    body = "".join(f"<{k}>{v}</{k}>" for k, v in extra.items())
    return (
        '<call name="send"><target_id>user</target_id>'
        f"<type>inquiry</type><text>{text}</text><kind>{kind}</kind>{body}</call>"
    )


DONE = '<call name="done"></call>'


async def _wire(kernel, script: _Script, *, answer: str, ai_id: str = "asker"):
    """A `user` the operator has told what to say, and an AI granted the edge to
    ask it. Two-sided deny-all: the asker's egress AND the human's ingress."""
    await kernel.send(
        [
            {
                "target": "kernel",
                "payload": {
                    "type": "create_agent",
                    "parent_id": kernel.id,
                    "handler_module": "user_agent.tools",
                    "id": "user",
                    "answer": answer,
                    "callable_by": {ai_id: "*"},
                },
            }
        ]
    )
    r = (
        await kernel.send(
            [
                {
                    "target": "kernel",
                    "payload": {
                        "type": "create_agent",
                        "parent_id": kernel.id,
                        "handler_module": "ollama_backend.tools",
                        "id": ai_id,
                        "context_window": 32768,
                        "can_call": {"user": "*", "cli": "*"},
                    },
                }
            ]
        )
    )[0]["result"]
    ot._providers[ai_id] = script
    return r["id"]


async def _tally(kernel, uid: str = "user") -> dict:
    return (await kernel.send([{"target": uid, "payload": {"type": "inquiries"}}]))[0][
        "result"
    ]


# ─── the tests ──────────────────────────────────────────────────


async def test_an_inquiry_is_a_call_and_the_answer_is_its_reply(seeded_kernel):
    """THE WHOLE CONTRACT IN ONE MESSAGE PAIR. The model asks; the human's answer
    comes back as that call's reply, tagged like any other; the model reads it
    and finishes. Scored on the human's own count, which only its handler can
    raise."""
    script = _Script([_ask("Proceed?", kind="confirm"), "understood " + DONE])
    aid = await _wire(seeded_kernel, script, answer="yes, proceed")
    try:
        await seeded_kernel.send(
            [{"target": aid, "payload": {"type": "send", "text": "set it up"}}]
        )
        await asyncio.sleep(0.5)

        tally = await _tally(seeded_kernel)
        assert tally["inquiries"] == 1, f"the human was never asked: {tally}"

        assert script.calls == 2, f"the answer did not become a message: {script.calls}"
        second = script.incoming(1)
        assert second.startswith("[reply to "), second
        assert " user inquiry" in second, second
        assert "yes, proceed" in second, second
    finally:
        ot._providers.pop(aid, None)


async def test_a_message_that_only_talks_about_asking_never_reaches_the_human(
    seeded_kernel,
):
    """THE REASON THE SCORE IS THE HUMAN'S TALLY AND NOT THE PROSE. A model can
    say it asked. The desk's count is the only thing it cannot fake — and the
    call-less message earns a `try again` rather than ending the chain."""
    script = _Script(
        ["I should ask the user whether to proceed.", "asking now " + DONE]
    )
    aid = await _wire(seeded_kernel, script, answer="yes")
    try:
        await seeded_kernel.send(
            [{"target": aid, "payload": {"type": "send", "text": "set it up"}}]
        )
        await asyncio.sleep(0.5)

        assert (await _tally(seeded_kernel))["inquiries"] == 0, "prose reached the desk"
        assert script.calls == 2, "a call-less message must be told to try again"
        assert "[no call was dispatched]" in script.incoming(1)
    finally:
        ot._providers.pop(aid, None)


async def test_a_proposal_is_held_by_the_human_not_run_by_the_sender(seeded_kernel):
    """AN `action` IS A PROPOSAL, not an act. A sender that may not perform
    something hands it over — "buy this, if you agree" — and the human HOLDS it.
    Read back off the call the desk really received, never off the model's
    words."""
    script = _Script(
        [
            _ask(
                "Buy it?",
                kind="single",
                action="<verb>purchase</verb><sku>SKU-9</sku>",
            ),
            "held " + DONE,
        ]
    )
    aid = await _wire(seeded_kernel, script, answer="go ahead")
    try:
        await seeded_kernel.send(
            [{"target": aid, "payload": {"type": "send", "text": "restock"}}]
        )
        await asyncio.sleep(0.5)

        tally = await _tally(seeded_kernel)
        assert tally["inquiries"] == 1, tally
        assert tally["proposals"] == [{"verb": "purchase", "sku": "SKU-9"}], tally
    finally:
        ot._providers.pop(aid, None)


async def test_an_unwired_human_fails_loud_instead_of_inventing_consent(
    seeded_kernel,
):
    """NO DEFAULT. A `user` with no `answer` on its record does not invent one —
    the model reads a refusal it can act on, and the tally stays at zero because
    nothing was answered."""
    script = _Script([_ask("Proceed?"), "noted " + DONE])
    aid = await _wire(seeded_kernel, script, answer="")
    try:
        await seeded_kernel.send(
            [{"target": aid, "payload": {"type": "send", "text": "set it up"}}]
        )
        await asyncio.sleep(0.5)

        assert (await _tally(seeded_kernel))["inquiries"] == 0
        second = script.incoming(1)
        assert "not_connected" in second or "no `answer`" in second, second
    finally:
        ot._providers.pop(aid, None)


async def test_the_gate_stands_between_a_model_and_a_person(seeded_kernel):
    """A HUMAN IS GATED LIKE ANY AGENT. An asker with no edge to `user` is
    refused, reads why, and the desk is never touched."""
    script = _Script([_ask("Proceed?"), "refused " + DONE])
    await seeded_kernel.send(
        [
            {
                "target": "kernel",
                "payload": {
                    "type": "create_agent",
                    "parent_id": seeded_kernel.id,
                    "handler_module": "user_agent.tools",
                    "id": "user",
                    "answer": "yes",
                },
            }
        ]
    )
    r = (
        await seeded_kernel.send(
            [
                {
                    "target": "kernel",
                    "payload": {
                        "type": "create_agent",
                        "parent_id": seeded_kernel.id,
                        "handler_module": "ollama_backend.tools",
                        "id": "stranger",
                        "context_window": 32768,
                        "can_call": {"cli": "*"},
                    },
                }
            ]
        )
    )[0]["result"]
    aid = r["id"]
    ot._providers[aid] = script
    try:
        await seeded_kernel.send(
            [{"target": aid, "payload": {"type": "send", "text": "set it up"}}]
        )
        await asyncio.sleep(0.5)

        assert (await _tally(seeded_kernel))["inquiries"] == 0
        assert "unauthorized" in script.incoming(1), script.incoming(1)
    finally:
        ot._providers.pop(aid, None)
