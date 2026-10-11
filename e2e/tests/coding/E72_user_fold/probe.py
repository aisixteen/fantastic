"""coding/user_fold — folding on a person's say-so, any time, with NO extra verb.

The reads are PLANTED (rules, then the fillers). The human's squeeze is the agent's `context_window` at 9000 for
the first live turn (below that the system block alone trips `too_small` and nothing folds); the watermark fires on
that turn and folds; once a fold is in the store the window is restored through the existing `update_agent`. Scored: a
fold landed while the window was shrunk, and the four rules still bind at the end.
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
    WINDOW,
    base_checks,
    call,
    conclude,
    file_reads,
    fixture,
    plant_fillers,
    plant_store,
    planted_reads,
    read_reply,
    report,
    rules_for,
    run_arm,
    score_rules,
    store_rows,
)
from helpers.kernel import open_kernel

BK = _resolve_backend()
DONE = 'end your run by calling done — emit <call name="done"></call>'
FOLD_ONLY = (FOLD_ARMS[1],)

FB = "ufold_fb"
N = 12
KEYS = ("owner", "lane")
SQUEEZED = 9000  # the floor: below it the system block alone trips `too_small`


async def main():
    root, core, close = await open_kernel(prefix="coding_ufold_", reach=[FB, "notes", "ai_recency", "ai_fold", "cli", "user"])
    try:
        tok = {k: secrets.token_hex(4) for k in (*KEYS, "first", "last")}
        rules = rules_for(tok, KEYS)
        Path("rules.md").write_text("# Rules\n\n" + "\n".join(f"- {d}:\n\n      {ln}\n" for d, ln in rules))
        plant_fillers(N, pad_to=3000)
        task = (f"Read `rules.md`, then `filler_0.txt` … `filler_{N - 1}.txt` through `notes`, ONE PER TURN. Then write "
                f"`report.md` through `notes` listing every filler, obeying every rule. Then {DONE}.")
        await fixture(root, FB, arms=FOLD_ARMS)
        results = []
        reads = file_reads(["rules.md"] + [f"filler_{i}.txt" for i in range(N)])
        for aid, strategy, extra in FOLD_ARMS:
            # THE PAST, PLANTED — each arm its own copy; the last reply is delivered live
            rows = planted_reads(root.ctx.mint_id, task, reads)
            await plant_store(core, root, aid, rows, band=None)  # its own squeeze is the subject

            async def _restore(aid=aid):
                while not any(m.get("kind") == "fold" for m in await store_rows(root, core, aid)):
                    await asyncio.sleep(2.0)
                await call(root, root.id, {"type": "update_agent", "id": aid, "context_window": WINDOW, "num_ctx": WINDOW})
            rs = asyncio.create_task(_restore())
            path, text = reads[-1]
            tail = read_reply(path, text, rows[-1]["id"])
            # the agent is created squeezed: its first live turn is the one that folds
            results.append(await run_arm(root, core, BK, FB, aid, strategy, extra, tail, "report.md",
                                         via=None, window=SQUEEZED))
            rs.cancel()
        recency, fold = results
        kept, _ = score_rules(fold["body"], rules)
        report((recency, fold), lambda a: f"RULES KEPT={score_rules(a['body'], rules)[0]}/4  folded={a['folded']}")
        # the control is never squeezed, so "both arms compacted" is not a precondition here
        checks = [c for c in base_checks(recency, fold) if not c[0].startswith(("both arms produced", "both arms actually compacted"))] + [
            (f"a fold landed while the window was squeezed ({fold['folded']})", fold["folded"] >= 1),
            (f"all four rules still bind ({kept}/4)", kept == 4),
        ]
        allok, note = conclude(recency, fold, checks, "folded on request, rules held", "a user_fold check failed")
        verdict(allok, note)
    finally:
        await close()


if __name__ == "__main__":
    asyncio.run(main())
