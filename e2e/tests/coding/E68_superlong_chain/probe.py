"""coding/superlong_chain — ONE chain that never closes, far past the window.

A single open chain has no closed subtree to fold, so the fold strategy folds its closed
PREFIX in spans, keeping only the live tail. Scored from the store and the last view:
several folds written, and the final view holds no row that any fold covers — after the
cut the model sees folds and the tail, nothing else. The report must still name (almost)
every file, which means the summaries carried them or the model went into a fold.
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

FB = "long_fb"
N = 40
TASK = (f"Read `filler_0.txt` … `filler_{N - 1}.txt` ({N} files) through `notes`, ONE PER TURN. Then write "
        f"`report.md` through `notes` listing every file you read, one per line. Then {DONE}.")


async def main():
    root, core, close = await open_kernel(prefix="coding_long_", reach=[FB, "notes", "ai_recency", "ai_fold", "cli", "user"])
    try:
        plant_fillers(N, pad_to=4000)
        await fixture(root, FB, arms=FOLD_ARMS)
        recency, fold = await both_arms(root, core, BK, FB, TASK, "report.md", arms=FOLD_ARMS)
        covered = {r for m in fold["msgs"] if m.get("kind") == "fold" for r in (m.get("refs") or [])}
        leaked = [i for i in fold["view_ids"] if i in covered]
        listed = sum(f"filler_{i}.txt" in fold["body"] for i in range(N))
        report((recency, fold), lambda a: f"listed={sum(f'filler_{i}.txt' in a['body'] for i in range(N))}/{N}  view rows={len(a['view_ids'])}")
        checks = [c for c in base_checks(recency, fold) if not c[0].startswith("both arms produced")] + [
            (f"several span folds were written ({fold['folded']})", fold["folded"] >= 3),
            (f"the last view holds no folded row ({len(leaked)} leaked)", not leaked),
            (f"the report names nearly every file ({listed}/{N})", listed >= int(N * 0.9)),
        ]
        allok, note = conclude(recency, fold, checks, "folded its own past in spans; view = folds + tail", "a superlong_chain check failed")
        verdict(allok, note)
    finally:
        await close()


if __name__ == "__main__":
    asyncio.run(main())
