"""coding/pull_by_choice — the model chooses what to read, and reads only that.

Every filler read is digested (async). spec.md says which ONE filler carries the
checksum the report needs. Scored on the answer AND on the set of pids pulled: pulling
the right body is by choice; pulling four is paging with extra steps.
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
DONE = 'end your run by calling done — emit <tool_call>{"name":"done"}</tool_call>'

FB = "choice_fb"
N = 8
TASK = (
    "Step 1: read `spec.md` through `notes`. Step 2: read `filler_0.txt` … "
    f"`filler_{N - 1}.txt` through `notes`, ONE PER TURN, to put them on record. "
    "Step 3: write `report.md` through `notes` with the single line "
    f"`checksum: <value>` as spec.md defines it. Then {DONE}."
)


async def main():
    root, core, close = await open_kernel(prefix="coding_choice_", reach=[FB, "notes", "ai_recency", "ai_fold", "cli", "user"])
    try:
        token = secrets.token_hex(4); which = 5
        Path("spec.md").write_text(
            f"# Spec\n\nThe checksum the report must carry is the value on the line "
            f"beginning `>> checksum:` inside `filler_{which}.txt`. The other fillers "
            "carry nothing you need.\n")
        plant_fillers(N, extra={which: f">> checksum: {token}"})
        await fixture(root, FB, arms=FOLD_ARMS)
        recency, fold = await both_arms(root, core, BK, FB, TASK, "report.md", arms=FOLD_ARMS)
        want = f"checksum: {token}"
        report((recency, fold), lambda a: f"FOUND={want in a['body']}  pulls={a['pulls']}  greps={a['greps']}")
        # not a compaction test: no "both arms compacted" precondition
        checks = [c for c in base_checks(recency, fold) if not c[0].startswith(("both arms produced", "both arms actually compacted"))] + [
            ("the fold arm reported the checksum exactly", want in fold["body"]),
            (f"it pulled at most 6 pages — the one body, not the corpus ({fold['pulls']})", fold["pulls"] <= 6),
        ]
        allok, note = conclude(recency, fold, checks, "chose one body, pulled it, reported", "a pull_by_choice check failed")
        verdict(allok, note)
    finally:
        await close()


if __name__ == "__main__":
    asyncio.run(main())
