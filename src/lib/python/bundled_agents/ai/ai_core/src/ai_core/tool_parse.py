"""Tool-call parsing — retained ONLY for the context-reaction read-model.

The backend no longer runs an agentic loop and does NOT parse tool calls at
send time: the loop is emergent (an authored agent drives tool dispatch). What
remains here is the non-streaming `extract_tool_calls`, used by
`core._derive_reaction` to scan the DURABLE transcript for whether the model
reacted to a compaction notice (e.g. by emitting a `recall`/persist call). The
envelope it recognizes (Hermes-style):

    <tool_call>{"name": "send", "arguments": {"target_id": "...", "payload": {...}}}</tool_call>
"""

from __future__ import annotations

import json
import secrets

OPEN = "<tool_call>"
CLOSE = "</tool_call>"


def _mint_id() -> str:
    return f"call_{secrets.token_hex(4)}"


def parse_one(inner: str) -> dict | None:
    """Parse the JSON between one tag pair into `{id, name, arguments}`.

    Lenient (tiny models drift): accepts `{"name","arguments"}`, a `tool` alias for
    `name`, a flattened object (remaining keys become arguments), and double-encoded
    (stringified) arguments. Returns None on unparseable JSON.
    """
    try:
        obj = json.loads(inner.strip())
    except (json.JSONDecodeError, ValueError):
        return None
    if not isinstance(obj, dict):
        return None
    name = obj.get("name") or obj.get("tool") or "send"
    args = obj.get("arguments")
    if args is None:
        args = {k: v for k, v in obj.items() if k not in ("name", "tool", "arguments")}
    if isinstance(args, str):
        try:
            args = json.loads(args)
        except (json.JSONDecodeError, ValueError):
            args = {}
    if not isinstance(args, dict):
        args = {}
    return {"id": _mint_id(), "name": str(name), "arguments": args}


def extract_tool_calls(text: str) -> list[dict]:
    """Non-streaming: pull every finalized `<tool_call>` out of a complete string.
    Used by the durable-history reader (`core._derive_reaction`)."""
    out: list[dict] = []
    i = 0
    while True:
        a = text.find(OPEN, i)
        if a == -1:
            break
        b = text.find(CLOSE, a + len(OPEN))
        if b == -1:
            break
        call = parse_one(text[a + len(OPEN) : b])
        if call is not None:
            out.append(call)
        i = b + len(CLOSE)
    return out
