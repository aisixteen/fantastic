"""coding/fold_paging — N folds, choose the right one, open only it.

Sixteen tiny tasks, PLANTED as finished roots (each read one file with its own checksum
line and ended in `done`); the first live turn folds them. The final message asks for
task #3's checksum. Scored:
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
    base_checks,
    closed_roots,
    conclude,
    file_reads,
    fixture,
    plant_fillers,
    plant_store,
    report,
    run_arm,
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
            # THE PAST, PLANTED — each arm its own copy: tasks 0…N-1 done, one root each
            past = [(task(i), file_reads([f"filler_{i}.txt"]),
                     f"filler_{i}.txt carries `>> checksum: {toks[i]}`.") for i in range(N)]
            await plant_store(core, root, aid, closed_roots(root.ctx.mint_id, past))
            results.append(await run_arm(root, core, BK, FB, aid, strategy, extra, final, "final.md"))
        recency, fold = results
        want = toks[ask]
        right_fold = None
        for m in fold["msgs"]:
            if m.get("kind") == "fold" and any(f"filler_{ask}" in str(r.get("content", "")) for r in fold["msgs"] if r.get("id") in (m.get("refs") or [])):
                right_fold = m.get("id")
        opened = set(fold["descents"])
        report((recency, fold), lambda a: f"FINAL={want in a['body']}  folded={a['folded']}  descents={len(a['descents'])}  roots={a['roots']}")
        print(f"  observed: fold covering task {ask} = {right_fold}; opened = {sorted(opened)}")
        checks = [c for c in base_checks(recency, fold, planted=True) if not c[0].startswith("both arms produced")] + [
            # 32k-only test (see test.yaml): at 32k the 16 tasks fold 3 times (measured
            # 2026-09-08); paging through folds needs more than one, not many.
            (f"a fold was written ({fold['folded']}, levels {fold['fold_levels']})", fold["folded"] >= 1),
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
