"""The `user` role — THE HUMAN, as an ordinary agent.

The premise is that AI and humans are first-class agents on one spine. An inquiry is
not a TEXT CONVENTION scraped by a harness regex and answered from outside the
kernel: the human has an id, a record, a reflect entry, ledger rows and a gate, so
a model can discover that a human exists.

Here it is an agent:

  * `send`     — this agent dispatches ANYTHING to ANYONE, as itself. Work
                 (`{type:"send", text}`), an inquiry of its own
                 (`{type:"inquiry", text}`), a kernel verb — one verb, because
                 every agent here is first-class and none is limited to a
                 narrower repertoire than the others. Gated, and the ledger row
                 names this agent: the wiring a test exercises is the wiring
                 production uses.
  * `inquiry`  — someone sends one; it is ACCEPTED at once, the human READS it
                 and replies LATER (fully async) — its own
                 send into the asker's conversation, tagged `[reply to <pid> ·
                 user inquiry]`, exactly as an AI that was asked answers. No
                 suspend, no resume, no scraping.

The answer is POLICY ON THE RECORD (`answer`), not code. One bundle serves every
arm: a test that wants a specific answer writes a different record. There is no
default — an unwired `user` fails loud rather than inventing a human's words.

A human also HOLDS AUTHORITY A MODEL CANNOT RAISE. `grants` on the record is a
list of kernel payloads applied on an inquiry, before answering — the declarative
form of "the person opened the door for you". That is a human's real power in
this substrate, and expressing it as data keeps one bundle serving every arm.

The production twin of this agent is a HUD bundle at the same id, answering the
same verb. The model cannot tell them apart, which is the point.
"""

from __future__ import annotations

import asyncio

from ai_core.tool_parse import render_reply
from kernel import current_origin, current_task

SENTENCE = (
    "The human. Send me an inquiry when you cannot proceed without a decision "
    "that is mine to make; I read it and reply later, under its pid."
)
#: How long the human takes to read an inquiry before replying.
READ_DELAY = 1.0
VERBS = {
    "inquiry": (
        "args: text:str (req), kind:str? (text|single|multi|confirm), "
        "options:list?, action:dict?, request_id:str?. ACCEPTED at once "
        "({accepted, re, request_id}); the answer arrives LATER as this "
        "agent's message under the inquiry's pid. Nothing suspends. "
        "An `action` is a PROPOSAL the sender may not perform itself: it is "
        "HELD, never run here."
    ),
    "inquiries": "no args — {inquiries, proposals}: how many inquiries this "
                 "agent has received, and every `action` one handed it. The "
                 "tally, not prose.",
    "send": (
        "args: target:str (req), payload:dict (req). Dispatches ANY payload to "
        "ANY agent as THIS agent — work, an inquiry of its own, a kernel verb. "
        "Gated like any send; the ledger row names this agent as the sender."
    ),
}

#: The four shapes a UI can render. `kind` is advisory here — this human answers
#: the same way regardless — but it is validated so a model learns the vocabulary
#: the real HUD will enforce.
KINDS = ("text", "single", "multi", "confirm")

#: HOW MANY INQUIRIES THIS AGENT ACTUALLY RECEIVED, per agent id. The scorer
#: reads THIS, never the model's prose: a turn that only talks about it leaves the
#: tally at zero. The other fixtures count their side effects for the same
#: reason — a model cannot fake the desk's own tally.
_RECEIVED: dict[str, int] = {}

#: PROPOSALS the human was handed, per agent id. A question may carry an
#: `action` the sender may not perform itself — "buy this, if you agree". The
#: human HOLDS it; pressing the button is a separate act by the person. The
#: runner reads the proposal from HERE, off a call the human actually received,
#: instead of scraping it out of the model's prose.
_PROPOSALS: dict[str, list] = {}


def _refuse(verb: str, msg: str, **extra) -> dict:
    return {"error": f"user_probe: {verb}: {msg}", "reason": "bad_arg", **extra}


async def handler(id: str, payload: dict, agent) -> dict | None:
    verb = payload.get("type")
    if verb in ("boot", "shutdown"):
        return None
    if verb == "reflect":
        return {"id": id, "sentence": SENTENCE, "inquiries": _RECEIVED.get(id, 0)}

    if verb == "inquiry":
        q = payload.get("text")
        if not isinstance(q, str) or not q.strip():
            return _refuse("inquiry", "`text` must be a non-empty string",
                           arg="text")
        kind = payload.get("kind", "text")
        if kind not in KINDS:
            return _refuse("inquiry", f"`kind` must be one of {list(KINDS)}",
                           arg="kind")
        rec = agent.get(id) or {}
        answer = rec.get("answer")
        # NO DEFAULT. A human's words are never invented: an unwired `user`
        # says so instead of fabricating consent (see the hard rule — a missing
        # precondition fails loud, it does not fall back).
        if not isinstance(answer, str) or not answer:
            return {
                "error": "user_probe: no `answer` on this agent's record — "
                         "the operator wires what the human says",
                "reason": "not_connected",
            }
        # AUTHORITY FIRST, THEN THE ANSWER. A model cannot raise its own reach
        # (`side:"self-grant"` is refused); a human can, and does it here before
        # replying, so "granted — retry" is true by the time the model reads it.
        rec_grants = rec.get("grants")
        applied = 0
        if isinstance(rec_grants, list):
            for g in rec_grants:
                if not isinstance(g, dict):
                    continue
                # THE ROOT'S REAL ID, not the `kernel` alias: the alias routes
                # a dispatch, but the gate resolves the target first, so a grant
                # written against `kernel` is refused naming the real root.
                out = await agent.send([{"target": agent.ctx.root.id, "payload": g}])
                err = (out[0].get("result") or {}).get("error")
                # A HUMAN THAT COULD NOT OPEN THE DOOR MUST NOT SAY IT DID.
                # Answering "granted" over a refused grant sends the model back
                # to a wall that never moved, and it reads as the model failing
                # to retry (e.g. `update_agent` is PRIVILEGED and a `"*"` grant
                # does not cover it).
                if err:
                    return {
                        "error": f"user_probe: grant refused, so the answer "
                                 f"would be false: {err}",
                        "reason": "unauthorized",
                        "grant": g,
                    }
                applied += 1
        action = payload.get("action")
        if isinstance(action, dict):
            _PROPOSALS.setdefault(id, []).append(action)
        _RECEIVED[id] = _RECEIVED.get(id, 0) + 1
        # A PERSON READS, THEN REPLIES — later, as their own send into the asker's
        # conversation under the inquiry's pid (gated like any send: `human()`
        # grants this agent `can_call` on every AI it serves).
        asker, pid = current_origin(), current_task()
        text = f"[reply to {pid} · {id} inquiry]\n" + render_reply(
            {"answer": answer, "request_id": payload.get("request_id")}
        )
        reply_client = payload.get("reply_client_id") or "cli"

        async def _reply():
            await asyncio.sleep(READ_DELAY)
            await agent.send([{"target": asker, "payload": {
                "type": "send", "text": text, "client_id": reply_client}}])

        agent.ctx.spawn_task(id, "inquiry_answer", _reply())
        return {"accepted": True, "re": pid, "request_id": payload.get("request_id"),
                "inquiries": _RECEIVED[id], "granted": applied}

    if verb == "inquiries":
        return {"inquiries": _RECEIVED.get(id, 0),
                "proposals": _PROPOSALS.get(id, [])}

    if verb == "send":
        target = payload.get("target")
        body = payload.get("payload")
        if not isinstance(target, str) or not target:
            return _refuse("send", "`target` must be a non-empty string",
                           arg="target")
        if not isinstance(body, dict) or not body.get("type"):
            return _refuse("send", "`payload` must be a dict naming a `type`",
                           arg="payload")
        # THE SENDER IS THIS AGENT. A handler runs with `_current_sender` set to
        # the agent it belongs to, so this is a gated `user -> <target>` send
        # with its own ledger row — not an operator injection from outside.
        out = await agent.send([{"target": target, "payload": body}])
        return {"sent": target, "result": out[0].get("result")}

    return {"error": f"user_probe: unknown verb {verb!r}", "reason": "unhandled",
            "verb": verb}
