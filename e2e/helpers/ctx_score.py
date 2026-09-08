"""Scoring the Context Protocol from a probe: what the MODEL did, not what the task said.

Every check here exists because the obvious one is a tautology. `E51_compaction` asserted
`"recall" in str(history)` and `secret in str(history)` — and BOTH were already true before
the model acted: the task text names `{type:'recall', query:'passphrase'}`, the task is a
user turn in the durable store, and the secret arrives in the Step-1 read reply. Measured
on `runs/20260904-044955/coding.compaction:swift.log`: recall returned `{"messages":[],
"total":0}`, the chain died, `answer.md` was never written, `last_projection` was
`{fired: False, too_small: True}` — and the test scored PASS.

So the rules here are:
  * scan ASSISTANT turns for calls; a call the TASK described is not a call the model made;
  * score a fact by WHICH reply carried it, not by its presence anywhere in the transcript;
  * prove compaction HAPPENED before scoring what survived it.
"""

from __future__ import annotations

import json

# REUSED, not re-derived. `core` already owns the reply-block grammar (`_BLOCK_SPLIT`
# splits a batched turn into blocks, `_REPLY_TAG` reads `<agent> <verb>` off one) — the
# same parser the projection trimmers run on. A second regex here would be a second
# grammar to keep in step with the first.
from ai_core.core import _REPLY_TAG, _reply_blocks
from ai_core.tool_parse import extract_tool_calls


def _content(m: dict) -> str:
    c = m.get("content")
    return c if isinstance(c, str) else json.dumps(c, default=str)


def calls_made(
    messages: list[dict], *, target: str | None = None, verb: str | None = None
) -> list[dict]:
    """The `send` tool-calls the MODEL actually emitted, from its own assistant turns.

    Reads the same `<tool_call>` envelopes the runtime dispatches, via the kernel's own
    parser — so a call the model only described in prose does not count, and a call the
    TASK text quoted does not count either (a task is a user turn, never scanned here)."""
    out = []
    for m in messages:
        if m.get("role") != "assistant":
            continue
        for call in extract_tool_calls(_content(m)):
            args = call.get("arguments") or {}
            payload = args.get("payload")
            if not isinstance(payload, dict):
                continue
            if target is not None and args.get("target_id") != target:
                continue
            if verb is not None and payload.get("type") != verb:
                continue
            out.append({"target": args.get("target_id"), "payload": payload,
                        "title": args.get("title")})
    return out


def reply_blocks(messages: list[dict], *, agent: str, verb: str) -> list[str]:
    """The text of every reply block produced by `<agent> <verb>`.

    Splitting on the reply tag is what separates "the store contains X" from "the call
    that was supposed to produce X produced it" — the difference E51 could not see."""
    out = []
    for m in messages:
        if m.get("role") == "assistant":
            continue  # replies arrive as user turns; assistant turns are the model's own
        for block in _reply_blocks(_content(m)):
            tag = _REPLY_TAG.match(block)
            if tag and tag.group(1) == agent and tag.group(2) == verb:
                out.append(block)
    return out


def recalled(messages: list[dict], self_id: str, needle: str) -> tuple[bool, bool]:
    """(the model called `recall` on itself, that recall's REPLY carried `needle`).

    The second half is the load-bearing one: it is false when recall returned an empty
    page, which is exactly the run that used to pass."""
    called = bool(calls_made(messages, target=self_id, verb="recall"))
    returned = any(
        needle in b for b in reply_blocks(messages, agent=self_id, verb="recall")
    )
    return called, returned


def fired(conn) -> bool:
    """Did compaction actually happen at any point in the run?

    From the pushed `context:compacted` events, NOT `context_status` — that reports only
    the LAST projection and is overwritten every turn, so a run that compacted five times
    and ended on a turn that fit reads `fired: False`."""
    return bool(conn.compactions)


def dropped_total(conn) -> int:
    return sum(int(d.get("dropped_turns") or 0) for d in conn.compactions)
