"""coding/fold_long — a run long enough that folds fire more than once.

Rules arrive in four files spread through 24 filler reads at a 12k window, so closed
stretches of the chain fold repeatedly. Scored: all eight rules bind at the end AND at
least two fold rows were written AND each has refs, a title and a body. `folded == 0`
voids the run — then nothing was at risk.

PRE-SEEDED: the reads are PLANTED — the instruction, each read call and
its reply inline — and the last reply arrives live, so the run continues that chain: the
folding and the answer are the model's own, without spending a live turn per file.
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
    both_planted,
    conclude,
    file_reads,
    fixture,
    plant_fillers,
    report,
    rules_for,
    score_rules,
)
from helpers.kernel import open_kernel

BK = _resolve_backend()
DONE = 'end your run by calling done — emit <call name="done"></call>'

FB = "foldlong_fb"
N = 24
KEYS = ("owner", "build", "rev", "checksum", "region", "lane")
TASK = (
    "Work through this sequence through `notes`, ONE file per turn: read `rules_a.md`, "
    "then filler_0..filler_5; read `rules_b.md`, then filler_6..filler_11; read `rules_c.md`, "
    "then filler_12..filler_17; read `rules_d.md`, then filler_18..filler_23. Every rules file "
    "adds rules that bind the final report; all apply. Then write `report.md` through `notes` "
    f"listing every filler you read, one per line, obeying every rule. Do not re-read rules. Then {DONE}."
)


async def main():
    root, core, close = await open_kernel(prefix="coding_foldlong_", reach=[FB, "notes", "ai_recency", "ai_fold", "cli", "user"])
    try:
        tok = {k: secrets.token_hex(4) for k in (*KEYS, "first", "last")}
        rules = rules_for(tok, KEYS)
        for f, chunk in zip("abcd", (rules[0:2], rules[2:4], rules[4:6], rules[6:8])):
            Path(f"rules_{f}.md").write_text(f"# Rules ({f})\n\nThese bind `report.md` for the rest of the run:\n\n"
                                            + "\n".join(f"- {d}:\n\n      {ln}\n" for d, ln in chunk))
        plant_fillers(N)
        assert not any(v in TASK for v in tok.values())
        await fixture(root, FB, arms=FOLD_ARMS)
        # the sequence the instruction names: each rules file, then its six fillers
        order = [p for f, lo in zip("abcd", (0, 6, 12, 18))
                 for p in (f"rules_{f}.md", *(f"filler_{i}.txt" for i in range(lo, lo + 6)))]
        recency, fold = await both_planted(root, core, BK, FB, TASK, file_reads(order), "report.md", arms=FOLD_ARMS)
        report((recency, fold), lambda a: f"RULES KEPT={score_rules(a['body'], rules)[0]}/8  folded={a['folded']}  fold rows={len(a['fold_ids'])}")
        well_formed = all(m.get("refs") and m.get("title") and m.get("body") for m in fold["msgs"] if m.get("kind") == "fold")
        checks = [c for c in base_checks(recency, fold, planted=True) if not c[0].startswith("both arms produced")] + [
            (f"a fold was written ({fold['folded']}, levels {fold['fold_levels']})", fold["folded"] >= 1),
            ("every fold row has refs, a title and a body", well_formed),
            (f"all eight rules still bind ({score_rules(fold['body'], rules)[0]}/8)", score_rules(fold["body"], rules)[0] == 8),
        ]
        allok, note = conclude(recency, fold, checks, "folded in batches, rules held", "a fold_long check failed")
        verdict(allok, note)
    finally:
        await close()


if __name__ == "__main__":
    asyncio.run(main())
