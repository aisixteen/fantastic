"""WATCH — examine every model turn as it lands, and stop the run when it goes wrong.

The problem this solves is wall-clock, not correctness. A live probe costs 10-20
minutes, and this session repeatedly spent all of it on a run that was already dead
at turn 2: a swallowed timeout (0 chars, scored as a model failure), a model
re-sending one reflect forever, 31k characters of `k e k e k e`. In every case the
evidence was in the FIRST few turns and nobody looked until the end.

So: after each assistant turn, run the tripwires. If one trips, the caller cancels
the in-flight task and scores immediately with the reason named.

Design rules, learned the hard way this session:

- A tripwire must not fire on legitimate work. The first no-progress cap counted
  only WRITES and cut a model at pass 10 while it was reading its inputs for the
  first time — reading is progress when you have not read it yet. Every tripwire
  here fires on REPETITION or DEGENERATION, never on "hasn't finished yet".
- A tripwire names what it saw. "capped" tells you nothing; "same call 4x:
  file_bridge_38de25 reflect" tells you the model is stuck in a retry loop.
- Tripwires are ADVISORY by default (`WATCH_STOP=0` reports without stopping), so
  a new one can be trialled against a run you already trust before it is armed.

Knobs, all env, all off when 0:

    WATCH_STOP        1 = stop on a trip (default), 0 = report only
    WATCH_DIGEST      1 = one compact line per turn (default), 0 = silent
    WATCH_REPEAT      identical consecutive calls before tripping   (default 4)
    WATCH_SILENT      consecutive turns with no call at all         (default 4)
    WATCH_MALFORMED   total unparseable envelopes before tripping   (default 3)
    WATCH_RATIO       unique-word ratio below which a turn is soup  (default 0.25)
"""

from __future__ import annotations

import json
import os
import re

STOP = os.environ.get("WATCH_STOP", "1") == "1"
DIGEST = os.environ.get("WATCH_DIGEST", "1") == "1"
REPEAT = int(os.environ.get("WATCH_REPEAT", "4"))
SILENT = int(os.environ.get("WATCH_SILENT", "4"))
MALFORMED = int(os.environ.get("WATCH_MALFORMED", "3"))
RATIO = float(os.environ.get("WATCH_RATIO", "0.25"))

# USE THE KERNEL'S OWN PARSER. A hand-rolled `<tool_call>(.*?)</tool_call>` regex
# spans ADJACENT envelopes — the capture starts at one opener and ends at a later
# closer, so the text begins with `<tool_call>` and `json.loads` rejects it. That
# reported 60 valid calls as malformed on 2026-08-24 and the tripwire below KILLED a
# healthy `live_build` run for "4 unparseable tool_call envelopes" that did not
# exist. A watchdog measuring a different parser than the one that dispatches is
# worse than no watchdog: it invents failures and ends runs that were fine.
import os
import sys

_PY = os.path.join(
    os.path.dirname(os.path.abspath(__file__)), "..", "src", "lib", "python"
)
# `ai_core` imports `kernel`, so the kernel root must be importable too.
for _p in (_PY, os.path.join(_PY, "bundled_agents/ai/ai_core/src")):
    if _p not in sys.path:
        sys.path.insert(0, _p)
from ai_core.tool_parse import extract_tool_calls, malformed_calls  # noqa: E402


def _calls(text: str) -> tuple[list[str], int]:
    """(signatures, malformed_count) for one turn, via `tool_parse` itself."""
    sigs = []
    for c in extract_tool_calls(text or ""):
        a = c.get("arguments") or {}
        sigs.append(json.dumps([a.get("target_id"), a.get("payload")], sort_keys=True))
    return sigs, len(malformed_calls(text or ""))


def _soup_ratio(text: str, window: int = 150) -> float | None:
    """Lowest unique-word ratio over any window. None if the turn is too short.

    This is the signature of a degenerating turn — `skimskim -> sksksk -> kkkk`
    drives the ratio toward zero long before the turn ends.

    CALIBRATED ON REAL TURNS, 2026-08-24, not guessed (every window >= 150 words):

        live_build, PASS      n=31   min 0.427   p5 0.473   median 0.600
        live_plan,  PASS      n=34   min 0.327   p5 0.440   median 0.603
        live_subai, degenerate       0.040

    The worst turn in a passing run sits 8x above the degenerate one, so 0.25 has
    room on both sides. Do not raise it toward 0.33 to catch more: that is inside
    the observed spread of real work, and a tripwire that cuts a passing run is
    worse than no tripwire — it converts a result into a mystery."""
    words = (text or "").split()
    if len(words) < window:
        return None
    worst = 1.0
    for i in range(0, len(words) - window, 25):
        w = words[i : i + window]
        worst = min(worst, len(set(x.lower() for x in w)) / len(w))
    return worst


class Watch:
    """Per-turn examination. `turn()` returns a stop reason, or None."""

    def __init__(self, label: str = "run"):
        self.label = label
        self.n = 0
        # TOTAL, for reporting. The TRIP reads the streak below.
        self.malformed = 0
        # CONSECUTIVE turns whose envelopes all failed to parse. Every other
        # tripwire here is a streak that resets on progress; this one used to be
        # cumulative, which is a different claim entirely — it said "you have
        # written three bad envelopes ever" rather than "you are stuck". A model
        # that writes a bad escape, is told so by `ai_core`, fixes it and carries
        # on was still killed for it three occasions later. Measured on `subai`:
        # 7 valid calls in turns 1-2, then two bad turns, and the run died at
        # turn 4 of a permitted 50 with the model actively retrying.
        self.malformed_streak = 0
        self.silent_streak = 0
        self.repeat_streak = 0
        self.last_sig: str | None = None
        self.trips: list[str] = []

    def turn(self, text: str, seconds: float = 0.0) -> str | None:
        self.n += 1
        sigs, bad = _calls(text)
        self.malformed += bad
        # A turn that produced a USABLE call is progress, whatever else it also
        # botched: the model is moving, not looping.
        self.malformed_streak = self.malformed_streak + 1 if (bad and not sigs) else 0

        if sigs:
            self.silent_streak = 0
            # consecutive IDENTICAL calls — a retry loop, not exploration
            if len(sigs) == 1 and sigs[0] == self.last_sig:
                self.repeat_streak += 1
            else:
                self.repeat_streak = 0
            self.last_sig = sigs[-1] if len(sigs) == 1 else None
        else:
            self.silent_streak += 1
            self.repeat_streak = 0

        if DIGEST:
            head = [json.loads(s)[0] for s in sigs] if sigs else []
            print(
                f"  · t{self.n:<3} {len(text or ''):>7}c {seconds:>5.0f}s "
                f"{len(sigs)} call{'s' if len(sigs) != 1 else ' '}"
                f"{' bad=' + str(bad) if bad else ''}"
                f"  {','.join(str(h) for h in head[:4])}",
                flush=True,
            )

        why = self._trip(text, sigs)
        if why:
            self.trips.append(f"turn {self.n}: {why}")
            print(f"\n  ** WATCH TRIPPED — {why} **\n", flush=True)
            return why if STOP else None
        return None

    def _trip(self, text: str, sigs: list[str]) -> str | None:
        ratio = _soup_ratio(text)
        if ratio is not None and ratio < RATIO:
            return f"degenerate output (unique-word ratio {ratio:.2f} < {RATIO})"
        if REPEAT and self.repeat_streak >= REPEAT - 1:
            tgt = json.loads(self.last_sig)[0] if self.last_sig else "?"
            return f"same call {self.repeat_streak + 1}x in a row: {tgt}"
        if SILENT and self.silent_streak >= SILENT:
            return f"{self.silent_streak} turns with no call at all"
        if MALFORMED and self.malformed_streak >= MALFORMED:
            return (
                f"{self.malformed_streak} consecutive turns of unparseable "
                f"tool_call envelopes ({self.malformed} total)"
            )
        return None
