"""THE STREAM JUNCTION — the ONE place the e2e harness reads streaming parts.

A model's output streams to its addressees as `part_*` events: `part_start
{stream_id, source, kind, name?, target?, to?, re?}` → `part_delta {text}` →
`part_end {ok?}` | `part_abort {reason}`. A dispatched MESSAGE (`type: send`) carries
the `stream_id` it was previewed as; the receiving AI's header says `· stream <id>`.

Every probe asserts THROUGH this module — none parses part frames itself. It keeps a
DRAFT per stream id (its kind, addressee, anchor `re`, the text so far) and settles it:
`ended` (ok or not), `aborted` (with the reason), and `matched` once a delivered
message names it. In process it watches inboxes on the live kernel.
"""

from __future__ import annotations

import re
import time
from dataclasses import dataclass, field
from typing import Any

#: A call part that ENDED but whose final message has not arrived after this many
#: seconds is FAILED: only a successful generation dispatches,
#: so a stream can end cleanly and still never become a message.
SETTLE_SECONDS = 60.0

_HEADER_STREAM = re.compile(r"· stream ([^\s\]·]+)")


@dataclass
class Draft:
    stream_id: str
    kind: str
    source: str
    target: str | None = None
    to: list[str] | None = None
    re: Any = None
    text: str = ""
    ended: bool = False
    ended_at: float | None = None
    ok: bool | None = None
    aborted: str | None = None
    matched: bool = False


@dataclass
class StreamJunction:
    """Collect `part_*` events from the watched inboxes into drafts."""

    root: Any
    inboxes: list[str] = field(default_factory=list)
    drafts: dict[str, Draft] = field(default_factory=dict)

    @classmethod
    def watch(cls, root: Any, *agent_ids: str) -> "StreamJunction":
        """Mirror each agent's inbox into a private inbox this junction drains.
        In process only (the live kernel's watch)."""
        j = cls(root)
        for aid in agent_ids:
            sink = f"_stream_{aid}"
            root.ctx.ensure_inbox(sink)
            root.watch(aid, sink)
            j.inboxes.append(sink)
        return j

    def drain(self) -> list[dict]:
        """Read every pending event; fold `part_*` ones into drafts. Returns them."""
        seen: list[dict] = []
        for sink in self.inboxes:
            q = self.root.ctx.inboxes[sink]
            while not q.empty():
                ev = q.get_nowait()
                if isinstance(ev, dict) and str(ev.get("type", "")).startswith("part_"):
                    self._fold(ev)
                    seen.append(ev)
        return seen

    def _fold(self, ev: dict) -> None:
        sid = ev.get("stream_id")
        if not isinstance(sid, str):
            return
        d = self.drafts.get(sid)
        if ev["type"] == "part_start":
            self.drafts[sid] = Draft(
                stream_id=sid,
                kind=ev.get("kind", ""),
                source=ev.get("source", ""),
                target=ev.get("target"),
                to=ev.get("to"),
                re=ev.get("re"),
            )
        elif d is None:
            return
        elif ev["type"] == "part_delta":
            d.text += ev.get("text", "")
        elif ev["type"] == "part_end":
            d.ended, d.ok, d.ended_at = True, ev.get("ok"), time.monotonic()
        elif ev["type"] == "part_abort":
            d.aborted = ev.get("reason") or "aborted"

    def match(self, delivered: str) -> Draft | None:
        """Settle the draft a delivered message names (`· stream <id>` in its
        header). The message is authoritative; the draft is marked matched."""
        m = _HEADER_STREAM.search(delivered.split("\n", 1)[0])
        if not m:
            return None
        d = self.drafts.get(m.group(1))
        if d is not None:
            d.matched = True
        return d

    def failed(self, now: float | None = None) -> list[Draft]:
        """Drafts that will never become a message: aborted, ended not ok, or a call
        that ended and saw no final message within SETTLE_SECONDS."""
        now = time.monotonic() if now is None else now
        return [
            d
            for d in self.drafts.values()
            if d.aborted is not None
            or (d.ended and d.ok is False)
            or (
                d.kind == "call"
                and d.ended
                and not d.matched
                and d.ended_at is not None
                and now - d.ended_at > SETTLE_SECONDS
            )
        ]

    def summary(self) -> dict:
        ds = list(self.drafts.values())
        return {
            "parts": len(ds),
            "calls": sum(d.kind == "call" for d in ds),
            "ended": sum(d.ended for d in ds),
            "ended_not_ok": sum(d.ended and d.ok is False for d in ds),
            "aborted": sum(d.aborted is not None for d in ds),
            "matched": sum(d.matched for d in ds),
            "open": sum(not d.ended and d.aborted is None for d in ds),
            "failed": len(self.failed()),
        }
