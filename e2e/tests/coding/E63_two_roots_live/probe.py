"""coding/two_roots_live — two ROOTS interleave across turns, in a live agent.

The human sends task A, and while A's chain is running sends task B. B queues behind the
FIFO lock and starts a new root; A keeps arriving as delivered replies. Scored on disk
(each report lists only its own files) and on the STORE: the rows must form exactly two
parent chains with no cross-links — mechanical, prose cannot fake it.
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
    conclude,
    fixture,
    plant_fillers,
    report,
    send_later,
)
from helpers.kernel import open_kernel

BK = _resolve_backend()
DONE = 'end your run by calling done — emit <tool_call>{"name":"done"}</tool_call>'

from ai_core.folds import chains as _chains

FB = "roots_fb"
TASK_A = ("TASK A: read `filler_0.txt` … `filler_3.txt` through `notes`, ONE PER TURN, then "
          f"write `report_a.md` listing exactly those four filenames, one per line. Then {DONE}.")
TASK_B = ("TASK B (separate from anything else you are doing): read `filler_4.txt` … "
          "`filler_7.txt` through `notes`, ONE PER TURN, then write `report_b.md` listing "
          f"exactly those four filenames, one per line. Then {DONE}.")


async def main():
    root, core, close = await open_kernel(prefix="coding_roots_", reach=[FB, "notes", "ai_recency", "ai_fold", "cli", "user"])
    try:
        plant_fillers(8)
        await fixture(root, FB, arms=FOLD_ARMS)
        results = []
        for aid, strategy, extra in FOLD_ARMS:
            from helpers.facts_arm import run_arm
            later = await send_later(root, aid, TASK_B, 45.0)
            a = await run_arm(root, core, BK, FB, aid, strategy, extra, TASK_A, "report_a.md")
            later.cancel()
            pb = Path("report_b.md"); a["body_b"] = pb.read_text() if pb.exists() else ""
            if pb.exists(): pb.rename(f"{aid}__report_b.md")
            a["chains"] = len(_chains(a["msgs"]))
            results.append(a)
        recency, fold = results

        def clean(a):
            A, B = a["body"], a["body_b"]
            a_ok = all(f"filler_{i}.txt" in A for i in range(4)) and not any(f"filler_{i}.txt" in A for i in range(4, 8))
            b_ok = all(f"filler_{i}.txt" in B for i in range(4, 8)) and not any(f"filler_{i}.txt" in B for i in range(4))
            return a_ok, b_ok

        report((recency, fold), lambda a: f"A clean={clean(a)[0]}  B clean={clean(a)[1]}  chains={a['chains']}  roots={a['roots']}")
        # not a compaction test: no "both arms compacted" precondition
        checks = [c for c in base_checks(recency, fold) if not c[0].startswith(("both arms produced", "both arms actually compacted"))] + [
            ("report_a lists only A's files", clean(fold)[0]),
            ("report_b lists only B's files", clean(fold)[1]),
            (f"the store holds exactly two roots ({fold['roots']})", fold["roots"] == 2),
        ]
        allok, note = conclude(recency, fold, checks, "two roots interleaved, two clean reports", "a two_roots_live check failed")
        verdict(allok, note)
    finally:
        await close()


if __name__ == "__main__":
    asyncio.run(main())
