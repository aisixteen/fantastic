"""coding/superlong_chain — ONE chain that never closes, far past the window.

A single open chain has no closed subtree to fold, so the fold strategy folds its closed
PREFIX in spans, keeping only the live tail. Scored from the store and the last view:
several folds written, and the final view holds no row that any fold covers — after the
cut the model sees folds and the tail, nothing else. The report must still name (almost)
every file, which means the summaries carried them or the model went into a fold.

PRE-SEEDED (owner, 2026-10-07): the reads are PLANTED — the instruction, each read call and
its reply inline — and the last reply arrives live, so the run continues that chain: the
folding and the answer are the model's own, without spending a live turn per file.
The live reply CONTINUES the planted chain, so it is still ONE chain that never closed.
"""
from __future__ import annotations

import asyncio
import sys
from pathlib import Path

sys.path.insert(
    0,
    str(next(d for d in Path(__file__).resolve().parents if (d / ".e2e-root").exists())),
)
from backends import backend as _resolve_backend
from helpers import verdict
from helpers.facts_arm import (
    FIT_MANY,
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
FOLD_ONLY = (FOLD_ARMS[1],)

FB = "long_fb"
N = 40
TASK = (f"Read `filler_0.txt` … `filler_{N - 1}.txt` ({N} files) through `notes`, ONE PER TURN. Then write "
        f"`report.md` through `notes` listing every file you read, one per line. Then {DONE}.")


async def main():
    root, core, close = await open_kernel(prefix="coding_long_", reach=[FB, "notes", "ai_recency", "ai_fold", "cli", "user"])
    try:
        plant_fillers(N, pad_to=4000)
        await fixture(root, FB, arms=FOLD_ARMS)
        reads = file_reads([f"filler_{i}.txt" for i in range(N)])
        recency, fold = await both_planted(root, core, BK, FB, TASK, reads, "report.md", arms=FOLD_ARMS, band=FIT_MANY)
        covered = {r for m in fold["msgs"] if m.get("kind") == "fold" for r in (m.get("refs") or [])}
        leaked = [i for i in fold["view_ids"] if i in covered]
        listed = sum(f"filler_{i}.txt" in fold["body"] for i in range(N))
        report((recency, fold), lambda a: f"listed={sum(f'filler_{i}.txt' in a['body'] for i in range(N))}/{N}  view rows={len(a['view_ids'])}")
        checks = [c for c in base_checks(recency, fold, planted=True) if not c[0].startswith("both arms produced")] + [
            (f"a span fold was written ({fold['folded']}, levels {fold['fold_levels']})", fold["folded"] >= 1),
            (f"the last view holds no folded row ({len(leaked)} leaked)", not leaked),
            (f"the report names nearly every file ({listed}/{N})", listed >= int(N * 0.9)),
        ]
        allok, note = conclude(recency, fold, checks, "folded its own past in spans; view = folds + tail", "a superlong_chain check failed")
        verdict(allok, note)
    finally:
        await close()


if __name__ == "__main__":
    asyncio.run(main())
