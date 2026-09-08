"""coding/flat_roots — a superlong FLAT list: many small roots, no depth.

Each root closes after one read and folds; folds pile up past half the view and fold
into higher folds. The final message needs a value from (nearly) every root. Scored:
a level-2 fold exists with title and body, and the final lists most checksums.
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
    conclude,
    fixture,
    plant_fillers,
    report,
    run_arm,
    after_tasks,
    send_later,
    send_when,
    store_rows,
)
from helpers.kernel import open_kernel

BK = _resolve_backend()
DONE = 'end your run by calling done — emit <tool_call>{"name":"done"}</tool_call>'
FOLD_ONLY = (FOLD_ARMS[1],)

FB = "flat_fb"
N = 24


def task(i):
    return (f"TASK {i}: read `filler_{i}.txt` through `notes` and state its `>> checksum:` value in one "
            f"sentence — nothing to write. Then {DONE}.")


async def main():
    root, core, close = await open_kernel(prefix="coding_flat_", reach=[FB, "notes", "ai_recency", "ai_fold", "cli", "user"])
    try:
        toks = {i: secrets.token_hex(4) for i in range(N)}
        plant_fillers(N, extra={i: f">> checksum: {toks[i]}" for i in range(N)}, pad_to=2500)
        await fixture(root, FB, arms=FOLD_ARMS)
        final = (f"FINAL: write `final.md` through `notes` with one line per task, `task <i>: <checksum>`, for "
                 f"ALL {N} tasks you did. Then {DONE}.")
        results = []
        for aid, strategy, extra in FOLD_ARMS:
            laters = [await send_later(root, aid, task(i), 20.0 * i) for i in range(1, N)]
            laters.append(await send_when(root, core, aid, final, after_tasks(root, aid, list(laters)), max_wait=20.0 * N + 900))
            results.append(await run_arm(root, core, BK, FB, aid, strategy, extra, task(0), "final.md"))
            for t in laters: t.cancel()
        recency, fold = results
        got = sum(toks[i] in fold["body"] for i in range(N))
        report((recency, fold), lambda a: f"checksums listed={sum(toks[i] in a['body'] for i in range(N))}/{N}")
        checks = [c for c in base_checks(recency, fold) if not c[0].startswith("both arms produced")] + [
            (f"many folds ({fold['folded']})", fold["folded"] >= 4),
            (f"a fold of folds exists (levels {fold['fold_levels']})", max(fold["fold_levels"] or [1]) >= 2),
            ("every fold has a title and a body", all(m.get("title") and m.get("body") for m in fold["msgs"] if m.get("kind") == "fold")),
            (f"the final lists most checksums ({got}/{N})", got >= int(N * 0.8)),
        ]
        allok, note = conclude(recency, fold, checks, "flat roots folded, folds folded, answers recovered", "a flat_roots check failed")
        verdict(allok, note)
    finally:
        await close()


if __name__ == "__main__":
    asyncio.run(main())
