"""ONE verdict line, so a launcher parses a token instead of guessing.

The probes grew five dialects of "it worked": `RESULT: PASS`, `RESULT: ✅ …`,
`HARNESS: USABLE`, `COMBINED -> PASS`, `ARM=x -> PASS`. A runner that scrapes
those needs one branch per dialect, and a probe whose wording drifts silently
becomes unscoreable — it reads as a failure that never happened.

So every probe ALSO emits `E2E-VERDICT: PASS|FAIL — note`. The human line stays
exactly as it was; this is a second line for machines. A probe that emits none is
reported as `no verdict`, never guessed into a pass or a fail.
"""

from __future__ import annotations

MARKER = "E2E-VERDICT:"


def verdict(ok: bool, note: str = "") -> bool:
    print(f"\n{MARKER} {'PASS' if ok else 'FAIL'}{' — ' + note if note else ''}",
          flush=True)
    return ok
