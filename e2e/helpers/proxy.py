"""THE PROXY READER — the one wrapper that watches a model think.

Every probe carried its own copy of this class under the name `Spy`, ten copies
of ~60 lines, each with the same three jobs: print the turn, record it, and stop
a runaway. They drifted. The per-turn bound was added to one after a probe burned
31,627 characters of the same paragraph in a single pass making zero calls; the
`assistant`-skip that keeps `seen` exact was fixed in another after parallel tool
calls dropped one message per turn. Neither fix reached its siblings.

One copy now. The bounds it enforces are the ones each copy earned:

  * PER-TURN bound (chars and seconds) — a no-progress counter counts TURNS, so
    it is structurally blind to a model stuck INSIDE one turn.
  * DISTINCT-CALL progress — progress is doing something not done before, which
    covers reading a file for the first time, not only writing. Counting writes
    alone cut a build probe at pass 10 while it was legitimately exploring.
  * WATCH trip — the caller's own per-turn predicate, examined when the turn
    lands rather than when the run ends.
"""

from __future__ import annotations

import re
import time
from dataclasses import dataclass

W = 78


def box(title: str, body: str, mark: str = "│") -> None:
    """The transcript's one rendering. Hard-wrapped so a 200k-char turn stays
    readable in a log file rather than becoming one line."""
    print(f"\n{title}")
    for raw in (body or "").splitlines() or [""]:
        for i in range(0, len(raw), W):
            print(f"  {mark} {raw[i : i + W]}")


def distinct_calls(transcript: list[str]) -> int:
    """How many DISTINCT calls the model has emitted.

    Repetition never moves this number; novel work always does. That asymmetry is
    what lets a cap end a re-verify-forever loop without truncating slow work.
    """
    seen = set()
    for block in re.findall(
        r"<call\s+name=(.*?)</call>", "".join(transcript), re.DOTALL
    ):
        seen.add(" ".join(block.split()))
    return len(seen)



@dataclass
class Limits:
    """Every bound in one place, all overridable from the environment."""

    idle: float = 25.0
    max_turns: int = 40
    no_progress: int = 8
    turn_chars: int = 120_000
    turn_seconds: float = 600.0
    #: The model's REAL window. 32768 was a third of what the endpoint serves,
    #: and the shortfall did not read as "too small" — it read as the model
    #: failing: `subai` was killed at turn 18 by ai_core's `too_small` failsafe
    #: with half the window already spent on `max_tokens` output reserve.
    #: A budget is a knob to set correctly, not a limit to write a failsafe for.
    ctx: int = 262144
    show_system: bool = False

    @classmethod
    def from_env(cls, env, **defaults) -> Limits:
        d = {**defaults}
        g = lambda k, cast, cur: cast(env.get(k, cur))
        return cls(
            idle=g("IDLE", float, d.get("idle", 25.0)),
            max_turns=g("MAX_TURNS", int, d.get("max_turns", 40)),
            no_progress=g("NO_PROGRESS", int, d.get("no_progress", 8)),
            turn_chars=g("TURN_CHARS", int, d.get("turn_chars", 120_000)),
            turn_seconds=g("TURN_SECONDS", float, d.get("turn_seconds", 600.0)),
            ctx=g("CTX", int, d.get("ctx", 262144)),
            show_system=env.get("SHOW_SYSTEM") == "1",
        )


class ProxyReader:
    """Wraps a provider: prints what the model is given and what it answers,
    records the transcript, and trips on a runaway turn.

    `trips` is the shared list the drive loop reads — appending to it is how this
    reader says "cut the run and name why".
    """

    def __init__(self, inner, limits: Limits, *, trips: list[str], watch=None):
        self.inner = inner
        self.limits = limits
        self.trips = trips
        self.watch = watch
        self.n = 0
        self.folds = 0
        self.transcript: list[str] = []
        self._seen = 0  # messages already printed

    async def chat(self, messages):
        # A FOLD IS NOT A TURN. The agent summarises a span through the same
        # provider (`ai_core.core._make_summarizer`); it carries no call and is
        # not a chain step. Counted as a turn it inflated `n`, fed NO_PROGRESS
        # (flat_roots: ten folds in a row → capped at 0/24) and polluted the
        # transcript `distinct_calls` and scorers grep.
        if messages and messages[0].get("role") == "system" and str(
            messages[0].get("content", "")
        ).startswith("Summarize the conversation excerpt"):
            self.folds += 1
            out: list[str] = []
            async for chunk in self.inner.chat(messages):
                out.append(chunk if isinstance(chunk, str) else "")
                yield chunk
            box(f"── fold {self.folds} · SUMMARY ←", "".join(out), "▌")
            return
        self.n += 1
        L = self.limits
        if self.n == 1 and L.show_system and messages:
            box("SYSTEM BLOCK", messages[0].get("content", ""), "┊")
        for m in messages[self._seen :] if self._seen else messages:
            # The assistant turn is printed from the STREAM below, so skipping it
            # here lets `_seen` be exactly `len(messages)` instead of a `+1` guess
            # that broke on parallel tool calls and dropped one message per turn.
            if m["role"] == "assistant":
                continue
            if m["role"] == "system" and not L.show_system:
                continue
            box(f"── turn {self.n} · {m['role'].upper()} →", m.get("content", ""))
        self._seen = len(messages)

        out: list[str] = []
        grew, began = 0, time.monotonic()
        async for chunk in self.inner.chat(messages):
            piece = chunk if isinstance(chunk, str) else ""
            out.append(piece)
            grew += len(piece)
            yield chunk
            spent = time.monotonic() - began
            if (L.turn_chars and grew > L.turn_chars) or (
                L.turn_seconds and spent > L.turn_seconds
            ):
                self.trips.append(
                    f"turn {self.n}: {grew} chars in {spent:.0f}s "
                    f"(limit {L.turn_chars} chars / {L.turn_seconds:.0f}s)"
                )
                break
        text = "".join(out)
        self.transcript.append(text)
        # EXAMINE THE TURN NOW, not when the run ends.
        if self.watch is not None:
            if (why := self.watch.turn(text, time.monotonic() - began)) is not None:
                self.trips.append(why)
        box(f"── turn {self.n} · ASSISTANT ←", text, "▌")

    @property
    def text(self) -> str:
        return "".join(self.transcript)

    def sees_prompt(self) -> bool:
        """True: this source sits BETWEEN ai_core and the model, so it sees what
        the model was given as well as what it answered. The wire source cannot,
        and says so — see helpers/transcript.py."""
        return True
