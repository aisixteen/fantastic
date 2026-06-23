"""RAW tool-call parsing — the ONE shared layer that owns tool-calling.

Fantastic NEVER uses a provider's native tool API. Providers are pure raw-text
streamers; THIS module (used by `core._run` for every backend) teaches the format
in the prompt and parses the call back out of the model's text stream.

The envelope (Hermes-style — widely trained, unambiguous, stream-friendly):

    <tool_call>{"name": "send", "arguments": {"target_id": "...", "payload": {...}}}</tool_call>

Text OUTSIDE the tags is content shown to the user. Tags may repeat (a batch of
calls in one turn). `stream_tool_calls` wraps a provider's `AsyncIterator[str]` and
yields the SAME events `core._run` already consumes:
  - `str` — a content token (streamed live to the caller).
  - `{"tool_call": {"id", "name", "arguments"}}` — one finalized call.
"""

from __future__ import annotations

import json
import secrets
from typing import AsyncIterator, Union

OPEN = "<tool_call>"
CLOSE = "</tool_call>"


def render_tool_call(name: str, arguments: dict) -> str:
    """Serialize a call into the envelope — used in the prompt example and when
    persisting an assistant turn so the model sees its own prior call as text."""
    return OPEN + json.dumps({"name": name, "arguments": arguments}) + CLOSE


def _mint_id() -> str:
    return f"call_{secrets.token_hex(4)}"


def parse_one(inner: str) -> dict | None:
    """Parse the JSON between one tag pair into `{id, name, arguments}`.

    Lenient (tiny models drift): accepts `{"name","arguments"}`, a `tool` alias for
    `name`, a flattened object (remaining keys become arguments), and double-encoded
    (stringified) arguments. Returns None on unparseable JSON — caller surfaces the
    raw text as content so nothing is lost.
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


def _partial_open_len(buf: str) -> int:
    """Longest suffix of `buf` that is a proper prefix of OPEN (a tag possibly split
    across chunks). Hold it back rather than emit it as content."""
    for k in range(min(len(buf), len(OPEN) - 1), 0, -1):
        if buf.endswith(OPEN[:k]):
            return k
    return 0


async def stream_tool_calls(
    raw: AsyncIterator[str],
) -> AsyncIterator[Union[str, dict]]:
    """Wrap a provider's raw text stream → content tokens + finalized tool-calls.

    Buffers across chunks (a tag may split mid-token); content outside tags streams
    live; one `{"tool_call": ...}` is yielded per closed tag. Malformed JSON inside a
    tag is surfaced as content (never crashes). An unterminated open tag at EOF is
    surfaced as content too.
    """
    buf = ""
    inside = False
    async for piece in raw:
        # Pass-through for a pre-formed event dict ({"tool_call": {...}}). REAL
        # providers yield only `str` (see the Provider ABC) — this lets a test or a
        # future structured source feed the loop's event contract directly without
        # reintroducing any native provider tool API.
        if isinstance(piece, dict):
            if "tool_call" in piece:
                if buf and not inside:
                    yield buf
                    buf = ""
                yield piece
            continue
        if not isinstance(piece, str):
            continue
        buf += piece
        while True:
            if not inside:
                idx = buf.find(OPEN)
                if idx == -1:
                    hold = _partial_open_len(buf)
                    emit = buf[: len(buf) - hold]
                    if emit:
                        yield emit
                    buf = buf[len(buf) - hold :]
                    break
                if idx > 0:
                    yield buf[:idx]
                buf = buf[idx + len(OPEN) :]
                inside = True
            else:
                cidx = buf.find(CLOSE)
                if cidx == -1:
                    break  # need more to close the tag
                inner = buf[:cidx]
                buf = buf[cidx + len(CLOSE) :]
                inside = False
                call = parse_one(inner)
                if call is not None:
                    yield {"tool_call": call}
                else:
                    yield OPEN + inner + CLOSE  # malformed — keep as content
    # flush
    if inside:
        yield OPEN + buf  # unterminated tag — surface raw, lose nothing
    elif buf:
        yield buf
