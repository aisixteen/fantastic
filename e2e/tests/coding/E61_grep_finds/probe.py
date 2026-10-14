"""coding/grep_finds — find the line, don't page for it.

Without grep a model pages a body in small windows to find a single line. This plants the same needle — a
marker mid-body in one of eight IDENTICAL-LENGTH fillers — tells the model it may search,
and scores whether it grepped and how little it pulled. Pull count is the falsifier: paging
costs ≥16 re-reads; a grep costs one call.
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

FB = "grep_fb"
N = 8
TASK = (
    f"One of `filler_0.txt` … `filler_{N - 1}.txt` ({N} files, all the same size) carries "
    "a line beginning `>> audit marker:`. You do not need to read them all — read them "
    "through `notes` ONE PER TURN so they are on record, then FIND the marker (you can "
    "grep everything you have received) and write `report.md` through `notes` with the "
    f"single line `marker: <filename> <value>`. Then {DONE}."
)


async def main():
    root, core, close = await open_kernel(prefix="coding_grep_", reach=[FB, "notes", "ai_recency", "ai_fold", "cli", "user"])
    try:
        token = secrets.token_hex(4); hit = 3
        plant_fillers(N, marker_in=hit, marker=token)
        await fixture(root, FB, arms=FOLD_ARMS)
        recency, fold = await both_arms(root, core, BK, FB, TASK, "report.md", arms=FOLD_ARMS)
        want = f"marker: filler_{hit}.txt {token}"
        report((recency, fold), lambda a: f"FOUND={want in a['body']}  greps={a['greps']}  folds_opened={a['folds_opened']}")
        # not a compaction test: no "both arms compacted" precondition
        checks = [c for c in base_checks(recency, fold) if not c[0].startswith(("both arms produced", "both arms actually compacted"))] + [
            ("the fold arm reported file and value exactly", want in fold["body"]),
            ("it used grep at least once", fold["greps"] >= 1),
        ]
        allok, note = conclude(recency, fold, checks, "grepped the hit, reported", "a grep_finds check failed")
        verdict(allok, note)
    finally:
        await close()


if __name__ == "__main__":
    asyncio.run(main())
