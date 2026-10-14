"""coding/reboot_continuity — kill the process state, keep the store, continue.

Sixteen reads. After the eighth the agent is deleted and re-created on the same id: its
rows and view stay in its memory store (delete_agent keeps store data), while reply
bodies — held in RAM only — are gone. The boot is lazy; the first turn after it reads
the head and runs `context_recall`. Scored: the report lists all sixteen, reads after
the reboot do not repeat the first eight, and the post-reboot view is a subset of the
store.

PRE-SEEDED: the first eight reads are PLANTED — each read call and its
reply inline, then the model's `done` — the agent boots over them, and the reboot and the
second half run live.
"""
from __future__ import annotations

import asyncio
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
    file_reads,
    fixture,
    plant_fillers,
    plant_store,
    planted_reads,
    read_view,
    reboot_agent,
    store_rows,
)
from helpers.kernel import open_kernel

BK = _resolve_backend()
DONE = 'end your run by calling done — emit <call name="done"></call>'
AID, STRATEGY, EXTRA = FOLD_ARMS[1]

FB = "reboot_fb"
N = 16


async def main():
    root, core, close = await open_kernel(prefix="coding_reboot_", reach=[FB, "notes", "ai_recency", AID, "cli", "user"])
    try:
        plant_fillers(N, pad_to=3000)
        await fixture(root, FB, arms=FOLD_ARMS)
        from helpers import ctx_score, drive
        from helpers.facts_arm import connect_arm
        half = (f"Read ONLY `filler_0.txt` through `filler_7.txt` (eight files) through `notes`, ONE PER TURN. "
                f"Do NOT read filler_8 or beyond, do NOT write anything. After filler_7, {DONE}.")
        # PHASE ONE, PLANTED: the instruction, eight reads, the model's `done`.
        rows = planted_reads(root.ctx.mint_id, half, file_reads([f"filler_{i}.txt" for i in range(8)]),
                             tail=False)
        rows.append({"role": "assistant", "id": root.ctx.mint_id(), "parent": rows[-1]["id"],
                     "content": '<call name="done"></call>'})
        await plant_store(core, root, AID, rows)
        conn = await connect_arm(root, core, BK, FB, AID, STRATEGY, EXTRA)
        before = await store_rows(root, core, AID)
        reads_before = len(ctx_score.calls_made(before, target="notes", verb="read"))
        # ── REBOOT ───────────────────────────────────────────────────────────
        conn = await reboot_agent(root, core, BK, FB, AID, STRATEGY, EXTRA, watch_conn=conn)
        # WHAT WAS READ IS MEASURED, NOT ASSUMED. Phase one is bounded at 14 turns, so the
        # first instruction is usually unfinished; a second instruction that claims "you
        # already have 0..7" is false and the model rightly re-reads the gap. Name the files it has; ask for the rest.
        have = sorted({i for i in range(N) for c in ctx_score.calls_made(before, target="notes", verb="read")
                       if f"filler_{i}.txt" in str(c["payload"])})
        have_txt = ", ".join(f"filler_{i}.txt" for i in have) or "none"
        rest = (f"Continue. Files you ALREADY read (in your history — do NOT re-read them): {have_txt}. "
                f"Read every OTHER file of `filler_0.txt` … `filler_{N - 1}.txt` through `notes`, ONE PER TURN. "
                f"Then write `report.md` through `notes` listing ALL {N} files you read across both instructions, "
                f"one per line. Then {DONE}.")
        await drive(conn, rest, client_id=AID, via="user")
        await conn.close()
        # THE BOOT IS LAZY: the first turn after it read the head and rebuilt the view.
        view = await read_view(core, root, AID)
        store_ids = {m.get("id") for m in await store_rows(root, core, AID)}
        view_ok = bool(view) and all(e["id"] in store_ids for e in view["ids"])
        after = await store_rows(root, core, AID)
        reads_after = len(ctx_score.calls_made(after, target="notes", verb="read")) - reads_before
        rereads = sum(1 for c in ctx_score.calls_made(after[len(before):], target="notes", verb="read")
                      if any(f"filler_{i}.txt" in str(c["payload"]) for i in have))
        body = Path("report.md").read_text() if Path("report.md").exists() else ""
        listed = sum(f"filler_{i}.txt" in body for i in range(N))
        print(f"     reads before={reads_before} after={reads_after} re-reads of already-read={rereads}  listed={listed}/{N}  "
              f"post-reboot view={len((view or {}).get('ids') or [])} rows ok={view_ok}")
        checks = [
            ("the reboot produced a view over the surviving store", view_ok),
            (f"the report lists all files ({listed}/{N})", listed == N),
            (f"no re-reads of files it already had ({rereads})", rereads == 0),
            (f"reads after reboot ≈ the files it lacked ({reads_after})", reads_after <= N - len(have) + 1),
        ]
        for name, ok in checks:
            print(f"  [{'ok' if ok else 'XX'}] {name}")
        allok = all(ok for _, ok in checks)
        verdict(allok, "rebooted on the same store; continued without re-reading" if allok else "a reboot_continuity check failed")
    finally:
        await close()


if __name__ == "__main__":
    asyncio.run(main())
