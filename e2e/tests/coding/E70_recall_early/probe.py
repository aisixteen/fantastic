"""coding/recall_early — the durable store is whole after a long run.

The very first instruction carries a code. Sixteen reads later, several folds in, the
model is asked to quote it — recall pages the store where every row and every fold still
lives — and to find a marker that sits inside a folded read, by grep.

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
FOLD_ONLY = (FOLD_ARMS[1],)

FB = "recall_fb"
N = 16


async def main():
    root, core, close = await open_kernel(prefix="coding_recall_", reach=[FB, "notes", "ai_recency", "ai_fold", "cli", "user"])
    try:
        code = secrets.token_hex(4); marker = secrets.token_hex(4)
        task = (f"Session code: {code}. Read `filler_0.txt` … `filler_{N - 1}.txt` through `notes`, ONE PER TURN. "
                f"Then write `final.md` through `notes` with two lines: `code: <the session code from your very first "
                f"instruction — use recall if it is no longer in view>` and `marker: <the audit marker value one filler "
                f"carried — grep for it>`. Then {DONE}.")
        plant_fillers(N, marker_in=2, marker=marker)
        await fixture(root, FB, arms=FOLD_ARMS)
        reads = file_reads([f"filler_{i}.txt" for i in range(N)])
        recency, fold = await both_planted(root, core, BK, FB, task, reads, "final.md", arms=FOLD_ARMS)
        from helpers import ctx_score
        recalled = len(ctx_score.calls_made(fold["msgs"], target="ai_fold", verb="recall"))
        report((recency, fold), lambda a: f"code={code in a['body']}  marker={marker in a['body']}")
            # NOT "both arms actually compacted": folding here was a side effect of a
            # BLIND grep (2026-09-08: 12 greps, 12 zeros, then pulls filled the window).
            # With grep reaching stored bodies (fixed 2026-09-09) the model never needs
            # to hold them, so this test measures FACT SURVIVAL; folding is measured by
            # fold_paging / nested_folds / superlong_chain / user_fold.
        checks = [c for c in base_checks(recency, fold) if not c[0].startswith(("both arms produced", "both arms actually compacted"))] + [
            # the claim is REACH, not method: the store under the folds answered, by
            # recall or by grep — either proves nothing was lost
            (f"the store was reached through the folds (recall={recalled}, greps={fold['greps']})",
             recalled >= 1 or fold["greps"] >= 1),
            ("the session code came back exactly", f"code: {code}" in fold["body"]),
            ("the marker came back exactly", f"marker: {marker}" in fold["body"]),
        ]
        allok, note = conclude(recency, fold, checks, "recalled the first row and grepped the store", "a recall_early check failed")
        verdict(allok, note)
    finally:
        await close()


if __name__ == "__main__":
    asyncio.run(main())
