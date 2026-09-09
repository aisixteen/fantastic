"""coding/nested_folds — folds of folds, with a needle two levels down.

32 reads at a 12k window: spans fold, then folds fold. A marker sits in filler_2 — under
a level-1 fold that is itself under a level-2 fold by the end. Scored: a level-2 fold
exists, every fold has a summary, and the final names the marker — by grep through the
nesting, or because a summary carried it.
"""
from __future__ import annotations

import asyncio
import secrets
import sys
from pathlib import Path

sys.path.insert(
    0,
    str(next(d for d in Path(__file__).resolve().parents if (d / ".e2e-root").exists())),
)
from backends import backend as _resolve_backend
from helpers import verdict
from helpers.facts_arm import (
    FOLD_ARMS,
    base_checks,
    both_arms,
    conclude,
    fixture,
    plant_fillers,
    report,
)
from helpers.kernel import open_kernel

BK = _resolve_backend()
DONE = 'end your run by calling done — emit <call name="done"></call>'
FOLD_ONLY = (FOLD_ARMS[1],)

FB = "nest_fb"
N = 32


async def main():
    root, core, close = await open_kernel(prefix="coding_nest_", reach=[FB, "notes", "ai_recency", "ai_fold", "cli", "user"])
    try:
        marker = secrets.token_hex(4)
        task = (f"Read `filler_0.txt` … `filler_{N - 1}.txt` through `notes`, ONE PER TURN, noting anything unusual. "
                f"Then write `final.md` through `notes` with the line `marker: <filename> <value>` for the one file that "
                f"carried an audit marker (grep for it if it is no longer in view). Then {DONE}.")
        plant_fillers(N, marker_in=2, marker=marker, pad_to=4000)
        await fixture(root, FB, arms=FOLD_ARMS)
        recency, fold = await both_arms(root, core, BK, FB, task, "final.md", arms=FOLD_ARMS)
        want = f"marker: filler_2.txt {marker}"
        report((recency, fold), lambda a: f"FOUND={want in a['body']}")
        checks = [c for c in base_checks(recency, fold) if not c[0].startswith("both arms produced")] + [
            (f"a fold of folds exists (levels {fold['fold_levels']})", max(fold["fold_levels"] or [1]) >= 2),
            ("every fold has a title and a body", all(m.get("title") and m.get("body") for m in fold["msgs"] if m.get("kind") == "fold")),
            ("the marker came back exactly through the nesting", want in fold["body"]),
        ]
        allok, note = conclude(recency, fold, checks, "nested folds, summarized at every level, needle found", "a nested_folds check failed")
        verdict(allok, note)
    finally:
        await close()


if __name__ == "__main__":
    asyncio.run(main())
