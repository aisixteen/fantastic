"""The `user` role — THE HUMAN, as an ordinary agent.

The premise is that AI and humans are first-class agents on one spine, and the
human was the one party never modelled. Asking used to be a TEXT CONVENTION: the
model printed `{"type":"question"}` on a line and stopped, a harness regex found
it (`e2e/helpers/proxy.py`), and the answer was posted back in from outside the
kernel. The human had no id, no record, no reflect entry, no ledger row and no
gate — a model could not even discover that a human existed.

Here it is an agent:

  * `task`     — the human gives the AI its work. The send is issued BY this
                 agent, so it is gated and carries `user` as its sender: the
                 wiring the test exercises is the wiring production uses.
  * `question` — the AI asks, and the ANSWER IS THE REPLY. No suspend, no
                 resume, no scraping: an ordinary call whose result the model
                 reads in its next turn like any other.

The answer is POLICY ON THE RECORD (`answer`), not code. One bundle serves every
arm: a test that wants a specific answer writes a different record. There is no
default — an unwired `user` fails loud rather than inventing a human's words.

A human also HOLDS AUTHORITY A MODEL CANNOT RAISE. `grants` on the record is a
list of kernel payloads applied when asked, before answering — the declarative
form of "the person opened the door for you". That is a human's real power in
this substrate, and expressing it as data keeps one bundle serving every arm.

The production twin of this agent is a HUD bundle at the same id, answering the
same verb. The model cannot tell them apart, which is the point.
"""

from __future__ import annotations

SENTENCE = (
    "The human. Ask me when you cannot proceed without a decision that is "
    "mine to make; the answer comes straight back as the reply."
)
VERBS = {
    "question": (
        "args: question:str (req), kind:str? (text|single|multi|confirm), "
        "options:list?, action:dict?, request_id:str?. Asks, and RETURNS the "
        "answer — {answer, request_id}. An ordinary call: nothing suspends. "
        "An `action` is a PROPOSAL the asker may not perform itself: it is "
        "HELD, never run here."
    ),
    "asks": "no args — {asks, proposals}: how many times this agent has been "
            "asked, and every `action` a question handed it. The tally, not prose.",
    "task": (
        "args: target:str (req), text:str (req). The human hands work to "
        "another agent; the send carries THIS agent as its sender."
    ),
}

#: The four shapes a UI can render. `kind` is advisory here — this human answers
#: the same way regardless — but it is validated so a model learns the vocabulary
#: the real HUD will enforce.
KINDS = ("text", "single", "multi", "confirm")

#: HOW MANY TIMES THE HUMAN WAS ACTUALLY ASKED, per agent id. The scorer reads
#: THIS, never the model's prose: a turn that talks about asking leaves the tally
#: at zero. The other fixtures count their side effects for the same reason —
#: a model cannot fake the desk's own tally.
_ASKS: dict[str, int] = {}

#: PROPOSALS the human was handed, per agent id. A question may carry an
#: `action` the asker may not perform itself — "buy this, if you agree". The
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
        return {"id": id, "sentence": SENTENCE, "verbs": VERBS,
                "asks": _ASKS.get(id, 0)}

    if verb == "question":
        q = payload.get("question")
        if not isinstance(q, str) or not q.strip():
            return _refuse("question", "`question` must be a non-empty string",
                           arg="question")
        kind = payload.get("kind", "text")
        if kind not in KINDS:
            return _refuse("question", f"`kind` must be one of {list(KINDS)}",
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
                # to retry (measured 2026-09-04: `update_agent` is PRIVILEGED
                # and a `"*"` grant does not cover it, so the human's own edit
                # was refused while it cheerfully replied "access granted").
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
        _ASKS[id] = _ASKS.get(id, 0) + 1
        return {"answer": answer, "request_id": payload.get("request_id"),
                "asks": _ASKS[id], "granted": applied}

    if verb == "asks":
        return {"asks": _ASKS.get(id, 0),
                "proposals": _PROPOSALS.get(id, [])}

    if verb == "task":
        target = payload.get("target")
        text = payload.get("text")
        if not isinstance(target, str) or not target:
            return _refuse("task", "`target` must be a non-empty string",
                           arg="target")
        if not isinstance(text, str) or not text:
            return _refuse("task", "`text` must be a non-empty string",
                           arg="text")
        # THE SENDER IS THIS AGENT. A handler runs with `_current_sender` set to
        # the agent it belongs to, so this is a gated `user -> <target>` send
        # with its own ledger row — not an operator injection from outside.
        out = await agent.send([{
            "target": target,
            "payload": {"type": "send", "text": text,
                        "client_id": payload.get("client_id", "user")},
        }])
        return {"tasked": target, "result": out[0].get("result")}

    return {"error": f"user_probe: unknown verb {verb!r}", "reason": "unhandled",
            "verb": verb}
