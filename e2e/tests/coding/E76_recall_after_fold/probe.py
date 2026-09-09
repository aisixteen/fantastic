"""coding/recall_after_fold — fold, recall, keep working.

`fold_descent`'s fixture: a marker mid-body in filler_1, sixteen reads at a 12k window.
Scored: a recall row follows each fold batch, every self-reply the agent got back was
inline (the cap invariant — 0 dehydrated), and the marker is reported.
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
    fixture,
    plant_fillers,
    run_arm,
)
from helpers.kernel import open_kernel

BK = _resolve_backend()
DONE = 'end your run by calling done — emit <tool_call>{"name":"done"}</tool_call>'
AID, STRATEGY, EXTRA = FOLD_ARMS[1]

FB = "raf_fb"
N = 16


async def main():
    root, core, close = await open_kernel(prefix="coding_raf_", reach=[FB, "notes", "ai_recency", AID, "cli", "user"])
    try:
        marker = secrets.token_hex(4)
        plant_fillers(N, marker_in=1, marker=marker)
        await fixture(root, FB, arms=FOLD_ARMS)
        task = (f"Read `filler_0.txt` … `filler_{N - 1}.txt` through `notes`, ONE PER TURN, noting anything unusual. Then "
                f"write `report.md` through `notes`: list every file, and add `marker: <filename> <value>` for the one "
                f"file that carried an audit marker (grep for it if it is no longer in view). Then {DONE}.")
        a = await run_arm(root, core, BK, FB, AID, STRATEGY, EXTRA, task, "report.md")
        want = f"marker: filler_1.txt {marker}"
        print(f"     turns={a['turns']}  folded={a['folded']}  recalls={a['recall_rows']}  greps={a['greps']}  pulls={a['pulls']}  "
              f"self-replies dehydrated={a['dehydrated_self']}{(' ' + '; '.join(a.get('dehydrated_self_tags') or [])) if a['dehydrated_self'] else ''}  FOUND={want in a['body']}")
        checks = [
            (f"folds fired ({a['folded']})", a["folded"] >= 1),
            (f"a recall followed the folds ({a['recall_rows']})", a["recall_rows"] >= 1),
            (f"no self-reply was dehydrated ({a['dehydrated_self']})", a["dehydrated_self"] == 0),
            ("the marker came back exactly", want in a["body"]),
        ]
        for name, ok in checks:
            print(f"  [{'ok' if ok else 'XX'}] {name}")
        allok = all(ok for _, ok in checks)
        verdict(allok, "folded, recalled, answered inline" if allok else "a recall_after_fold check failed")
    finally:
        await close()


if __name__ == "__main__":
    asyncio.run(main())
