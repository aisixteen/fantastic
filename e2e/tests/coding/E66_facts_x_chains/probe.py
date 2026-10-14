"""coding/facts_x_chains — several roots over time, then reach back.

The human sends four tasks over time; each reads four fillers and writes its own report.
At a 12k window the early chains fold while later ones run. The final message asks for
the marker that was in task A's files. Scored: four clean reports, and the answer came
from a descent into A's fold (or its summary already carried it).
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
    after_tasks,
    send_later,
    send_when,
    store_rows,
)
from helpers.kernel import open_kernel

BK = _resolve_backend()
DONE = 'end your run by calling done — emit <call name="done"></call>'

FB = "fxc_fb"
LETTERS = "abcd"


def task(i, letter):
    lo = 4 * i
    return (f"TASK {letter.upper()}: read `filler_{lo}.txt` … `filler_{lo + 3}.txt` through `notes`, ONE PER TURN, "
            f"noting anything unusual; then write `report_{letter}.md` listing exactly those four filenames, "
            f"one per line, plus `marker: <value>` if one of them carried an audit marker. Then {DONE}.")


async def main():
    root, core, close = await open_kernel(prefix="coding_fxc_", reach=[FB, "notes", "ai_recency", "ai_fold", "cli", "user"])
    try:
        token = secrets.token_hex(4)
        plant_fillers(16, marker_in=1, marker=token)  # in task A's files
        await fixture(root, FB, arms=FOLD_ARMS)
        final = (f"FINAL: which value was the audit marker in TASK A's files? Write `final.md` through `notes` "
                 f"with the single line `marker: <value>`. Then {DONE}.")
        results = []
        for aid, strategy, extra in FOLD_ARMS:
            from helpers.facts_arm import run_arm
            laters = [await send_later(root, aid, task(i, LETTERS[i]), 40.0 * i) for i in range(1, 4)]
            _idle = after_tasks(root, aid, list(laters))  # a COPY: the FINAL must not wait for itself

            async def _reports_done():
                return all(Path(f"report_{L}.md").exists() for L in LETTERS) or await _idle()

            laters.append(await send_when(root, core, aid, final, _reports_done, max_wait=40.0 * 4 + 600))
            a = await run_arm(root, core, BK, FB, aid, strategy, extra, task(0, "a"), "final.md")
            for t in laters: t.cancel()
            a["reports"] = {}
            for L in LETTERS:
                p = Path(f"report_{L}.md"); a["reports"][L] = p.read_text() if p.exists() else ""
                if p.exists(): p.rename(f"{aid}__report_{L}.md")
            results.append(a)
        recency, fold = results

        def clean(a):
            ok = []
            for i, L in enumerate(LETTERS):
                b = a["reports"][L]; lo = 4 * i
                # `filler_{j}.txt`, not `filler_{j}`: the bare prefix made `filler_1` match
            # `filler_10..15`, so reports c and d could never be clean.
            ok.append(all(f"filler_{j}.txt" in b for j in range(lo, lo + 4)) and not any(f"filler_{j}.txt" in b for j in range(16) if not lo <= j < lo + 4))
            return ok

        in_fold_text = any(token in str(m.get("title", "")) + str(m.get("body", "")) for m in fold["msgs"] if m.get("kind") == "fold")
        report((recency, fold), lambda a: f"reports clean={clean(a)}  FINAL={token in a['body']}  folded={a['folded']}  descents={len(a['descents'])}")
            # NOT "both arms actually compacted": with grep reaching stored bodies the
            # model never needs to hold them, so this test measures FACT SURVIVAL; folding is measured by
            # fold_paging / nested_folds / superlong_chain / user_fold.
        checks = [c for c in base_checks(recency, fold) if not c[0].startswith(("both arms produced", "both arms actually compacted"))] + [
            (f"four clean reports ({sum(clean(fold))}/4)", all(clean(fold))),
            ("final.md carries task A's marker", token in fold["body"]),
        ]
        allok, note = conclude(recency, fold, checks, "four chains, four clean reports, reached back correctly", "a facts_x_chains check failed")
        verdict(allok, note)
    finally:
        await close()


if __name__ == "__main__":
    asyncio.run(main())
