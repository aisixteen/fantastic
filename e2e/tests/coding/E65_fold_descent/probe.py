"""coding/fold_descent — the answer is inside a fold; the model has to walk down.

A marker sits mid-body in filler_1 (early, so its stretch folds). The final step asks for
it. Scored on the exact answer and on whether the model DESCENDED (grep scoped to a fold
id, or a pull of one). If the fold's own summary already carries the value, descent was
not needed — reported, not failed.

PRE-SEEDED (owner, 2026-10-07): the reads are PLANTED — the instruction, each read call and
its reply inline — and the last reply arrives live, so the run continues that chain: the
folding and the answer are the model's own, without spending a live turn per file.
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
    both_planted,
    conclude,
    file_reads,
    fixture,
    plant_fillers,
    report,
)
from helpers.kernel import open_kernel

BK = _resolve_backend()
DONE = 'end your run by calling done — emit <call name="done"></call>'

FB = "descent_fb"
N = 16
TASK = (
    f"Read `filler_0.txt` … `filler_{N - 1}.txt` through `notes`, ONE PER TURN, noting anything "
    "unusual. Then write `report.md` through `notes`: list every file, and add the line "
    f"`marker: <filename> <value>` for the one file that carried an audit marker. Then {DONE}."
)


async def main():
    root, core, close = await open_kernel(prefix="coding_descent_", reach=[FB, "notes", "ai_recency", "ai_fold", "cli", "user"])
    try:
        token = secrets.token_hex(4); hit = 1
        plant_fillers(N, marker_in=hit, marker=token)
        await fixture(root, FB, arms=FOLD_ARMS)
        reads = file_reads([f"filler_{i}.txt" for i in range(N)])
        recency, fold = await both_planted(root, core, BK, FB, TASK, reads, "report.md", arms=FOLD_ARMS)
        want = f"marker: filler_{hit}.txt {token}"
        in_fold_text = any(token in str(m.get("title", "")) + str(m.get("body", "")) for m in fold["msgs"] if m.get("kind") == "fold")
        report((recency, fold), lambda a: f"FOUND={want in a['body']}  folded={a['folded']}  descents={len(a['descents'])}")
        print(f"  observed: the value {'IS' if in_fold_text else 'is NOT'} in a fold's own summary")
        checks = [c for c in base_checks(recency, fold, planted=True) if not c[0].startswith("both arms produced")] + [
            (f"folds fired ({fold['folded']})", fold["folded"] >= 1),
            ("the fold arm reported file and value exactly", want in fold["body"]),
            ("it descended into a fold, or the summary already carried it", len(fold["descents"]) >= 1 or in_fold_text),
        ]
        allok, note = conclude(recency, fold, checks, "walked into the fold and came back with it", "a fold_descent check failed")
        verdict(allok, note)
    finally:
        await close()


if __name__ == "__main__":
    asyncio.run(main())
