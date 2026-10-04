"""Streaming parts — a model's output split into PARTS as it is written (owner design
2026-10-04, `tmp/streaming_parts.md`).

The provider streams raw text; this splitter reads it incrementally, with the SAME
grammar `tool_parse` reads whole, and yields part lifecycle events:

    ("start", part)        a part opened — `part` is {stream_id, kind, name?, target?, to?}
    ("delta", part, text)  text of that part as it arrives
    ("end", part, info)    the part closed (`</think>`, `</call>`, or the pass ended on text);
                           `info` is {} — for a call, {"ok": bool}: whether the block
                           parsed (a malformed call is never dispatched)
    ("abort", part, why)   the part never closed: the pass ended, was cut, or was stopped

Kinds: "text" (prose), "thinking" (inside `<think>`), "call" (a `<call name="…">`
envelope, its args streaming as they are written). A call written inside a thought is a
call part — dispatch reads the full text, so an action mid-thought is still an action.

It is ANIMATION, never action: nothing here dispatches. The call parser in `tool_parse`
stays the one authority on what a call IS; this only tells a watcher what is being
written. A call part names its addressee once `<target_id>` (or `<to>`) has closed, so
its `start` is held until then — a watcher of that addressee sees the whole part.
"""

from __future__ import annotations

import secrets

from ai_core.tool_parse import BYTES_CLOSE, BYTES_OPEN, CLOSE, OPEN, parses

THINK_OPEN = "<think>"
THINK_CLOSE = "</think>"
_NAME_HEAD = 'name="'


def _held_tail(buf: str, tags: tuple[str, ...]) -> int:
    """How many trailing chars of `buf` could be the start of one of `tags` — held back
    until the next chunk says whether they are markup or prose."""
    best = 0
    for tag in tags:
        for n in range(min(len(tag) - 1, len(buf)), 0, -1):
            if tag.startswith(buf[-n:]):
                best = max(best, n)
                break
    return best


class PartSplitter:
    """Feed it chunks; it returns part events. One per model pass."""

    def __init__(self, source: str):
        self._source = source
        self._buf = ""  # unconsumed text
        self._in_think = False
        self._part: dict | None = None  # the open text/thinking part
        self._call: dict | None = None  # the open call part
        self._call_body = ""  # the call's body so far (for target detection)
        self._call_started = False  # its `start` was emitted
        self._call_pending = ""  # deltas held until the addressee is known
        self._in_bytes = False
        self.parts: list[dict] = []  # every part opened, in order
        # every CLOSED call part, in order: (stream_id, body) — what a dispatched
        # call is matched to by its exact body, so the message it becomes carries
        # the stream it was previewed as (owner, 2026-10-04)
        self.closed_calls: list[tuple[str, str]] = []

    # ── public ──
    def feed(self, chunk: str) -> list[tuple]:
        self._buf += chunk
        out: list[tuple] = []
        while self._step(out):
            pass
        return out

    def finish(self, why: str = "no_close_tag") -> list[tuple]:
        """The pass ended: flush prose, END an open text/thinking part, ABORT an open
        call (a call that never closed was never a call)."""
        out: list[tuple] = []
        if self._call is not None:
            self._call_body += self._buf
            self._buf = ""
            out += self._abort_call(why)
        else:
            if self._buf:
                self._text(out, self._buf)
                self._buf = ""
            if self._part is not None:
                if self._part["kind"] == "thinking":
                    out.append(("abort", self._part, why))
                else:
                    out.append(("end", self._part, {}))
                self._part = None
        return out

    def stop(self, why: str) -> list[tuple]:
        """Stopped mid-stream: every open part ABORTS (nothing is half-ended)."""
        out: list[tuple] = []
        if self._call is not None:
            out += self._abort_call(why)
        if self._part is not None:
            out.append(("abort", self._part, why))
            self._part = None
        self._buf = ""
        return out

    @property
    def open_call(self) -> dict | None:
        return self._call

    @property
    def in_think(self) -> bool:
        return self._in_think

    # ── internals ──
    def _new(self, kind: str, **extra) -> dict:
        part = {
            "stream_id": f"{self._source}:{secrets.token_hex(4)}",
            "kind": kind,
            **extra,
        }
        self.parts.append(part)
        return part

    def _text(self, out: list[tuple], text: str) -> None:
        """Prose or thought, outside any call."""
        if not text:
            return
        kind = "thinking" if self._in_think else "text"
        if self._part is None:
            if kind == "text" and not text.strip():
                return  # whitespace between parts is not a part
            self._part = self._new(kind)
            out.append(("start", self._part))
        out.append(("delta", self._part, text))

    def _close_text_part(self, out: list[tuple]) -> None:
        if self._part is not None:
            out.append(("end", self._part, {}))
            self._part = None

    def _step(self, out: list[tuple]) -> bool:
        if self._call is not None:
            return self._step_call(out)
        # Outside a call: look for the next markup that changes the part.
        opens = [OPEN, THINK_CLOSE if self._in_think else THINK_OPEN]
        hits = [(self._buf.find(t), t) for t in opens]
        hits = [(i, t) for i, t in hits if i != -1]
        if not hits:
            hold = _held_tail(self._buf, tuple(opens))
            self._text(out, self._buf[: len(self._buf) - hold])
            self._buf = self._buf[len(self._buf) - hold :]
            return False
        i, tag = min(hits)
        self._text(out, self._buf[:i])
        if tag == OPEN:
            rest = self._buf[i + len(OPEN) :]
            # `<call` + whitespace + `name="X">` — anything else is prose (`<caller>`).
            if not rest:
                self._buf = self._buf[i:]
                return False
            if not rest[0].isspace():
                self._text(out, OPEN)
                self._buf = rest
                return True
            head = rest.lstrip()
            if len(head) < len(_NAME_HEAD) or not head.startswith(_NAME_HEAD):
                if _NAME_HEAD.startswith(head):
                    self._buf = self._buf[i:]  # could still become a call
                    return False
                self._text(out, OPEN)
                self._buf = rest
                return True
            q = head.find('"', len(_NAME_HEAD))
            gt = head.find(">", q + 1) if q != -1 else -1
            if q == -1 or gt == -1:
                self._buf = self._buf[i:]
                return False
            self._close_text_part(out)
            self._call = self._new("call", name=head[len(_NAME_HEAD) : q])
            self._call_body = ""
            self._call_started = False
            self._call_pending = ""
            self._in_bytes = False
            self._buf = head[gt + 1 :]
            return True
        # a thinking boundary
        self._close_text_part(out)
        self._in_think = tag == THINK_OPEN
        self._buf = self._buf[i + len(tag) :]
        return True

    def _step_call(self, out: list[tuple]) -> bool:
        stop_tags = (BYTES_CLOSE,) if self._in_bytes else (BYTES_OPEN, CLOSE)
        hits = [(self._buf.find(t), t) for t in stop_tags]
        hits = [(i, t) for i, t in hits if i != -1]
        if not hits:
            hold = _held_tail(self._buf, stop_tags)
            self._call_text(out, self._buf[: len(self._buf) - hold])
            self._buf = self._buf[len(self._buf) - hold :]
            return False
        i, tag = min(hits)
        if tag == CLOSE:
            self._call_text(out, self._buf[:i])
            self._buf = self._buf[i + len(CLOSE) :]
            self._end_call(out)
            return True
        self._call_text(out, self._buf[: i + len(tag)])
        self._buf = self._buf[i + len(tag) :]
        self._in_bytes = tag == BYTES_OPEN
        return True

    def _call_text(self, out: list[tuple], text: str) -> None:
        if not text:
            return
        self._call_body += text
        if self._call_started:
            out.append(("delta", self._call, text))
            return
        self._call_pending += text
        self._learn_addressee()
        if self._addressee_known():
            self._call_started = True
            out.append(("start", self._call))
            out.append(("delta", self._call, self._call_pending))
            self._call_pending = ""

    def _learn_addressee(self) -> None:
        body = self._call_body
        for tag in ("target_id", "to"):
            a = body.find(f"<{tag}>")
            b = body.find(f"</{tag}>", a + 1) if a != -1 else -1
            if a != -1 and b != -1:
                inner = body[a + len(tag) + 2 : b]
                if tag == "target_id":
                    self._call["target"] = inner.strip()
                else:
                    items = [
                        x.split("</item>")[0].strip() for x in inner.split("<item>")[1:]
                    ]
                    self._call["to"] = [x for x in items if x]

    def _addressee_known(self) -> bool:
        if "target" in self._call or "to" in self._call:
            return True
        # Only `send` names an addressee; every other tool (done, await,
        # inquiry_answer) speaks to the caller from its first byte.
        return self._call.get("name") != "send"

    def _end_call(self, out: list[tuple]) -> None:
        if not self._call_started:
            self._call_started = True
            out.append(("start", self._call))
            if self._call_pending:
                out.append(("delta", self._call, self._call_pending))
        self._call_pending = ""
        ok = parses(f' name="{self._call["name"]}">{self._call_body}')
        self.closed_calls.append((self._call["stream_id"], self._call_body))
        out.append(("end", self._call, {"ok": ok}))
        self._call = None

    def _abort_call(self, why: str) -> list[tuple]:
        out: list[tuple] = []
        if self._call_started:
            out.append(("abort", self._call, why))
        # An unannounced call (its addressee never written) aborts silently:
        # nobody was shown its start.
        self._call = None
        self._call_pending = ""
        return out


def cut_marker(text_so_far: str, why: str, unsent: list[dict]) -> str:
    """The assistant row a CUT pass leaves (owner, 2026-10-04) — a marker on top of the
    context, never a half answer. `why` is "stopped by <receiver>" (a receiver stopped
    the stream) or "model stopped generating" (the pass ended with a block still open).
    Kept: the VISIBLE text streamed so far, thinking stripped; a cut inside an open
    thought says so; every call that did not go out is NAMED as not sent, never kept as
    markup."""
    splitter = PartSplitter("marker")
    events = splitter.feed(text_so_far)
    visible = "".join(
        ev[2] for ev in events if ev[0] == "delta" and ev[1]["kind"] == "text"
    ).strip()
    head = f"[{why}"
    if splitter.in_think and not visible:
        head += " while thinking"
    lines = [head + "]"]
    if visible:
        lines.append(visible)
    for call in unsent:
        to = call.get("target") or ", ".join(call.get("to") or []) or "the caller"
        lines.append(f"[a {call.get('name', 'call')} call to {to} was not sent]")
    return "\n".join(lines)
