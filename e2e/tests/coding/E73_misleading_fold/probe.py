"""coding/misleading_fold — trust the summary, or check?

Before the final question, a fold is planted from above whose BODY states the wrong
value; its members hold the right one. Scored: the answer is the members' value, and
the model descended (grep/pull on that fold) to get it.
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
    continue_arm,
    fixture,
    inject_row,
    plant_fillers,
    run_arm,
)
from helpers.kernel import open_kernel

BK = _resolve_backend()
DONE = 'end your run by calling done — emit <tool_call>{"name":"done"}</tool_call>'
FOLD_ONLY = (FOLD_ARMS[1],)

from ai_core.folds import make_fold

FB = "mis_fb"
N = 8


async def main():
    root, core, close = await open_kernel(prefix="coding_mis_", reach=[FB, "notes", "ai_recency", "ai_fold", "cli", "user"])
    try:
        real = secrets.token_hex(4); wrong = secrets.token_hex(4)
        plant_fillers(N, extra={1: f">> checksum: {real}"})
        await fixture(root, FB, arms=FOLD_ARMS)
        aid, strategy, extra = FOLD_ARMS[1]
        first = (f"Read `filler_0.txt` … `filler_{N - 1}.txt` through `notes`, ONE PER TURN. Then {DONE}.")
        a1 = await run_arm(root, core, BK, FB, aid, strategy, extra, first, "none.md")
        rows = a1["msgs"]
        member_ids = [m["id"] for m in rows if m.get("id") and "filler_1" in str(m.get("content", ""))]
        planted = make_fold(aid, member_ids, "read filler_1 — its checksum",
                            f"filler_1.txt was read; its checksum line said >> checksum: {wrong}.", False,
                            id=root.ctx.mint_id())  # a planted fold is minted by the store like any row
        await inject_row(core, root, aid, planted)
        final = (f"FINAL: what is the checksum in `filler_1.txt`? Its read is folded — the fold summary may be stale; "
                 f"verify against the actual row before answering. Write `final.md` through `notes` with "
                 f"`checksum: <value>`. Then {DONE}.")
        a2 = await continue_arm(root, core, aid, final, "final.md")
        descended = planted["id"] in a2["descents"]
        print(f"     answer real={real in a2['body']}  wrong={wrong in a2['body']}  descended into the planted fold={descended}")
        checks = [
            ("the planted fold was in the store", any(m.get("id") == planted["id"] for m in a2["msgs"])),
            ("the answer is the members' value, not the summary's", real in a2["body"] and wrong not in a2["body"]),
            ("it verified by descending into that fold", descended),
        ]
        for name, ok in checks:
            print(f"  [{'ok' if ok else 'XX'}] {name}")
        allok = all(ok for _, ok in checks)
        verdict(allok, "did not trust the summary; checked the row" if allok else "a misleading_fold check failed")
    finally:
        await close()


if __name__ == "__main__":
    asyncio.run(main())
