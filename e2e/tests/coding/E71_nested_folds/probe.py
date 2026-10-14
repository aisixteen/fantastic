"""coding/nested_folds — folds of folds, with a needle two levels down.

PRESYNTHESIZED: the 32 reads are PLANTED in each arm's memory store — the read calls and
their replies, the filler text inline — instead of spending 32 live turns producing
them. The subject is unchanged: the FOLD MECHANISM folds that long past — spans fold,
then folds fold — at a small window, and a marker sits in filler_2, under a level-1 fold
that may itself sit under a level-2 fold. Scored: the past folded (levels reported), every fold
has a summary, and the final names the marker — by grep through the nesting, or because
a summary carried it.
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
    FIT_MANY,
    FOLD_ARMS,
    base_checks,
    both_arms,
    conclude,
    fixture,
    plant_fillers,
    plant_store,
    report,
)
from helpers.kernel import open_kernel

BK = _resolve_backend()
DONE = 'end your run by calling done — emit <call name="done"></call>'

FB = "nest_fb"
N = 32


def planted_reads(mint) -> list[dict]:
    """The past a live run would have produced: the goal, then per filler the model's
    `read` call and the reply to it, the file's text INLINE (a planted history carries
    its replies inline — bodies are never persisted). Ids from the kernel's counter."""
    goal = {"role": "user", "id": mint(), "parent": None,
            "content": f"Read `filler_0.txt` … `filler_{N - 1}.txt` through `notes`, ONE PER TURN, "
                       "noting anything unusual."}
    rows, prev = [goal], goal["id"]
    for i in range(N):
        call_id, pid = mint(), mint()
        rows.append({"role": "assistant", "id": call_id, "parent": prev,
                     "content": f'Reading filler_{i}. <call name="send"><target_id>notes</target_id>'
                                f"<title>read filler_{i}</title><type>read</type>"
                                f"<path>filler_{i}.txt</path></call>"})
        text = Path(f"filler_{i}.txt").read_text()
        reply = {"role": "user", "id": mint(), "parent": call_id,
                 "content": f'[reply to {pid} · notes read · "read filler_{i}" · ↑{call_id}]\n'
                            f"<path>filler_{i}.txt</path>\n<text>{text}</text>"}
        rows.append(reply)
        prev = reply["id"]
    return rows


async def main():
    root, core, close = await open_kernel(prefix="coding_nest_", reach=[FB, "notes", "ai_recency", "ai_fold", "cli", "user"])
    try:
        marker = secrets.token_hex(4)
        plant_fillers(N, marker_in=2, marker=marker, pad_to=4000)
        await fixture(root, FB, arms=FOLD_ARMS)
        # THE PAST, PLANTED — each arm gets its own copy, under its own id.
        for aid, _strategy, _extra in FOLD_ARMS:
            await plant_store(core, root, aid, planted_reads(root.ctx.mint_id), band=FIT_MANY)
        task = (f"Your history holds your reads of `filler_0.txt` … `filler_{N - 1}.txt`. Write `final.md` through "
                f"`notes` with the line `marker: <filename> <value>` for the one file that carried an audit marker "
                f"(grep your memory for it if it is no longer in view — do not re-read the files). Then {DONE}.")
        recency, fold = await both_arms(root, core, BK, FB, task, "final.md", arms=FOLD_ARMS)
        want = f"marker: filler_2.txt {marker}"
        report((recency, fold), lambda a: f"FOUND={want in a['body']}")
        checks = [c for c in base_checks(recency, fold, planted=True) if not c[0].startswith("both arms produced")] + [
            (f"the past folded (levels {fold['fold_levels']} — nesting reported, not required)", bool(fold["fold_ids"])),
            ("every fold has a title and a body", all(m.get("title") and m.get("body") for m in fold["msgs"] if m.get("kind") == "fold")),
            ("the marker came back exactly through the nesting", want in fold["body"]),
        ]
        allok, note = conclude(recency, fold, checks, "nested folds, summarized at every level, needle found", "a nested_folds check failed")
        verdict(allok, note)
    finally:
        await close()


if __name__ == "__main__":
    asyncio.run(main())
