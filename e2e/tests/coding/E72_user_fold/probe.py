"""coding/user_fold — folding on a person's say-so, any time, with NO extra verb.

The human shrinks the agent's `context_window` through the existing `update_agent` mid-run (to 9000 — below that the
system block alone trips `too_small` and nothing folds);
the watermark fires on the next turn and folds; the window is then restored. Scored: a
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
    base_checks,
    call,
    conclude,
    fixture,
    plant_fillers,
    report,
    rules_for,
    run_arm,
    score_rules,
)
from helpers.kernel import open_kernel

BK = _resolve_backend()
DONE = 'end your run by calling done — emit <tool_call>{"name":"done"}</tool_call>'
FOLD_ONLY = (FOLD_ARMS[1],)

FB = "ufold_fb"
N = 12
KEYS = ("owner", "lane")


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
        for aid, strategy, extra in FOLD_ARMS:
            async def _squeeze(aid=aid):
                await asyncio.sleep(70.0)
                await call(root, root.id, {"type": "update_agent", "id": aid, "context_window": 9000, "num_ctx": 9000})
                await asyncio.sleep(60.0)
                await call(root, root.id, {"type": "update_agent", "id": aid, "context_window": 12000, "num_ctx": 12000})
            sq = asyncio.create_task(_squeeze())
            results.append(await run_arm(root, core, BK, FB, aid, strategy, extra, task, "report.md"))
            sq.cancel()
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
