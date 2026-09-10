"""An ORACLE — a fixed body of knowledge that answers QUESTIONS, deterministically.

One of the pieces of the multi-agent test: each solver has its own oracle, and the
oracle knows only its own ledger. It is NOT a model (no lane, exact, cheap): `ask`
is keyword search over the ledger lines on the record — every word of 3+ letters
in the question is matched case-insensitively, the lines that hit the MOST
keywords come back, at most `limit` of them. So a precise question ("which room is
Dana in") answers, and "tell me everything" returns noise, which is the point: the
solver must work out what to ask.

Knowledge is POLICY ON THE RECORD (`lines`), written by the fixture; the bundle
holds nothing. `asked` on reflect and `_ASKED` are the tally the scorer reads —
a solver that only talks about its oracle leaves it at zero.
"""

from __future__ import annotations

import re

SENTENCE = (
    "An oracle: a ledger of facts I answer questions about. Send me `ask` with a "
    "precise `question`; I return the ledger lines that match its words. I know "
    "only my own ledger."
)
VERBS = {
    "ask": (
        "args: question:str (req), limit:int? (default 6). Keyword search over my "
        "ledger — the lines matching the most words of the question come back as "
        "{answers:[str], matched:int}. Ask precisely; a vague question returns noise."
    ),
}

_ASKED: dict[str, list[str]] = {}


async def handler(id: str, payload: dict, agent) -> dict | None:
    verb = payload.get("type")
    if verb in ("boot", "shutdown"):
        return None
    if verb == "reflect":
        return {
            "id": id,
            "sentence": SENTENCE,
            "verbs": VERBS,
            "asked": len(_ASKED.get(id, [])),
        }
    if verb == "ask":
        q = payload.get("question")
        if isinstance(q, (int, float)) and not isinstance(q, bool):
            q = str(q)  # the tag grammar types a bare number; a number is a question too
        if not isinstance(q, str) or not q.strip():
            return {
                "error": "oracle_probe: ask: `question` must be a non-empty string",
                "reason": "bad_arg",
                "arg": "question",
            }
        rec = agent.get(id) or {}
        lines = rec.get("lines")
        if not isinstance(lines, list) or not lines:
            return {
                "error": "oracle_probe: no `lines` on this agent's record — the "
                "operator wires what this oracle knows",
                "reason": "not_connected",
            }
        _ASKED.setdefault(id, []).append(q)
        try:
            limit = max(1, min(int(payload.get("limit") or 6), 20))
        except (TypeError, ValueError):
            limit = 6
        words = {w.lower() for w in re.findall(r"[A-Za-z0-9_]{3,}", q)}
        scored = []
        for ln in lines:
            hits = sum(1 for w in words if w in str(ln).lower())
            if hits:
                scored.append((hits, str(ln)))
        scored.sort(key=lambda t: (-t[0], t[1]))
        return {
            "answers": [ln for _, ln in scored[:limit]],
            "matched": len(scored),
        }
    return {
        "error": f"oracle_probe: unknown verb {verb!r}; verbs: {sorted(VERBS)}",
        "reason": "unhandled",
    }
