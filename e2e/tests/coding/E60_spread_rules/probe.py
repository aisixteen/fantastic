"""coding/spread_rules — rules arrive in THREE files across a 12-filler run.

`fact_survival` front-loads: read the spec, then work. The howto says "dump the task
first", so a model that checkpoints once at the start and never again would still pass
it. Here that model fails: two rules arrive at the start, two more after filler_4, two
more after filler_8 — by which point the window has already been cut several times.
Checkpointing has to be a habit, not a first move. Twelve fillers push the cut count up.
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
    LOREM,
    base_checks,
    both_arms,
    conclude,
    fixture,
    report,
)
from helpers.kernel import open_kernel

BK = _resolve_backend()
FB = "spread_fb"
N = 12
DONE = 'end your run by calling done — emit <call name="done"></call>'
TASK = (
    "Work through this sequence through the editor `notes`, ONE file per turn:\n"
    "  read `rules_a.md`, then `filler_0.txt` … `filler_4.txt`,\n"
    "  read `rules_b.md`, then `filler_5.txt` … `filler_8.txt`,\n"
    "  read `rules_c.md`, then `filler_9.txt` … `filler_11.txt`.\n"
    "Every rules file adds rules that bind the final report; all of them apply.\n"
    "Then write `report.md` through `notes` listing every filler you read, one per line, "
    f"obeying every rule from all three files. Do not re-read any rules file. Then {DONE}."
)


async def main():
    root, core, close = await open_kernel(
        prefix="coding_spread_", reach=[FB, "notes", "ai_recency", "ai_fold", "cli", "user"]
    )
    try:
        t = {k: secrets.token_hex(4) for k in ("first", "owner", "build", "rev", "lane", "last")}
        rules = [
            ("a", "its FIRST line must be exactly", f"## {t['first']}"),
            ("a", "it must contain a line reading exactly", f"owner: {t['owner']}"),
            ("b", "it must contain a line reading exactly", f"build: {t['build']}"),
            ("b", "it must contain a line reading exactly", f"rev: {t['rev']}"),
            ("c", "it must contain a line reading exactly", f"lane: {t['lane']}"),
            ("c", "its LAST line must be exactly", f"{t['last']}-end"),
        ]
        for f in "abc":
            mine = [(d, ln) for ff, d, ln in rules if ff == f]
            Path(f"rules_{f}.md").write_text(
                f"# Rules ({f})\n\nThese bind `report.md` for the rest of the run:\n\n"
                + "\n".join(f"- {d}:\n\n      {ln}\n" for d, ln in mine))
        for i in range(N):
            Path(f"filler_{i}.txt").write_text(f"chunk {i}: {LOREM}\n")
        assert not any(v in TASK for v in t.values())
        await fixture(root, FB, arms=FOLD_ARMS)
        recency, facts = await both_arms(root, core, BK, FB, TASK, "report.md", arms=FOLD_ARMS)

        def score(a):
            lines = [ln.strip() for ln in a["body"].strip().splitlines()]
            kept = []
            for i, (_, _, ln) in enumerate(rules):
                ok = (bool(lines) and lines[0] == ln) if i == 0 else \
                     (bool(lines) and lines[-1] == ln) if i == len(rules) - 1 else ln in lines
                kept.append(ok)
            return sum(kept), [f for (f, _, _), k in zip(rules, kept) if not k]

        report((recency, facts), lambda a: (
            f"RULES KEPT={score(a)[0]}/6   lost from files={score(a)[1]}"))
        # Did checkpointing HAPPEN AFTER THE FIRST CUT — the thing this test exists for?
        # Did the model DISTIL rules that arrived after stripping had started (b/c)
        # into its own prose — the thing the design relies on instead of a dump?
        late = sum(1 for ff, _, ln in rules if ff in "bc" and any(
            m.get("role") == "assistant" and ln in str(m.get("content")) for m in facts["msgs"]))
            # NOT "both arms actually compacted": folding here was a side effect of a
            # BLIND grep (2026-09-08: 12 greps, 12 zeros, then pulls filled the window).
            # With grep reaching stored bodies (fixed 2026-09-09) the model never needs
            # to hold them, so this test measures FACT SURVIVAL; folding is measured by
            # fold_paging / nested_folds / superlong_chain / user_fold.
        checks = [c for c in base_checks(recency, facts) if not c[0].startswith(('both arms produced', 'both arms actually compacted'))] + [
            ("the fold arm produced the artifact", bool(facts['body'].strip())),
            ("the fold arm restated b/c rules in its own prose after they arrived",
             late > 0),
            (f"the fold arm kept all six ({score(facts)[0]}/6)", score(facts)[0] == 6),
        ]
        print(f"\n  MEASURED — rules kept:  recency={score(recency)[0]}/6  "
              f"facts={score(facts)[0]}/6   late checkpoints={late}")
        allok, note = conclude(recency, facts, checks, "kept checkpointing after cuts began; all six rules held", "a spread_rules check failed")
        verdict(allok, note)
    finally:
        await close()


if __name__ == "__main__":
    asyncio.run(main())
