"""coding/fold_paging — N folds, choose the right one, open only it.

Sixteen tiny tasks sent over time, each reads one file with its own checksum line and
finishes. Each closes and folds. The final message asks for task #3's checksum. Scored:
exact answer, the descended fold is the one covering task 3, and at most two folds were
opened — opening four is paging, not choosing.
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
    after_tasks,
    base_checks,
    conclude,
    fixture,
    plant_fillers,
    report,
    send_later,
    send_when,
)
from helpers.kernel import open_kernel

BK = _resolve_backend()
DONE = 'end your run by calling done — emit <call name="done"></call>'

FB = "paging_fb"
N = 16


def task(i):
    return (f"TASK {i}: read `filler_{i}.txt` through `notes` and reply with its `>> checksum:` line "
            f"in your own words — no file to write. Then {DONE}.")


async def main():
    root, core, close = await open_kernel(prefix="coding_paging_", reach=[FB, "notes", "ai_recency", "ai_fold", "cli", "user"])
    try:
        toks = {i: secrets.token_hex(4) for i in range(N)}
        plant_fillers(N, extra={i: f">> checksum: {toks[i]}" for i in range(N)}, pad_to=2500)
        await fixture(root, FB, arms=FOLD_ARMS)
        ask = 3
        final = (f"FINAL: what was the checksum in TASK {ask}'s file? Write `final.md` through `notes` with the "
                 f"single line `checksum: <value>`. Then {DONE}.")
        results = []
        for aid, strategy, extra in FOLD_ARMS:
            from helpers.facts_arm import run_arm
            laters = [await send_later(root, aid, task(i), 22.0 * i) for i in range(1, N)]
            laters.append(await send_when(root, core, aid, final, after_tasks(root, aid, list(laters)), max_wait=22.0 * N + 600))
            a = await run_arm(root, core, BK, FB, aid, strategy, extra, task(0), "final.md")
            for t in laters: t.cancel()
            results.append(a)
        recency, fold = results
        want = toks[ask]
        right_fold = None
        for m in fold["msgs"]:
            if m.get("kind") == "fold" and any(f"filler_{ask}" in str(r.get("content", "")) for r in fold["msgs"] if r.get("id") in (m.get("refs") or [])):
                right_fold = m.get("id")
        opened = set(fold["descents"])
        report((recency, fold), lambda a: f"FINAL={want in a['body']}  folded={a['folded']}  descents={len(a['descents'])}  roots={a['roots']}")
        print(f"  observed: fold covering task {ask} = {right_fold}; opened = {sorted(opened)}")
        checks = [c for c in base_checks(recency, fold) if not c[0].startswith("both arms produced")] + [
            # 32k-only test (see test.yaml): at 32k the 16 tasks fold 3 times (measured
            # 2026-09-08); paging through folds needs more than one, not many.
            (f"folds were written ({fold['folded']})", fold["folded"] >= 2),
            ("final.md carries task 3's checksum exactly", want in fold["body"]),
            ("the right fold was among those opened (or none needed)", not opened or right_fold in opened),
            (f"at most two folds opened ({len(opened)})", len(opened) <= 2),
        ]
        allok, note = conclude(recency, fold, checks, "chose the fold, opened it, answered", "a fold_paging check failed")
        verdict(allok, note)
    finally:
        await close()


if __name__ == "__main__":
    asyncio.run(main())
