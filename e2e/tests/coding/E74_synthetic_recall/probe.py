"""coding/synthetic_recall — the agent wakes into a history it never lived.

A realistic conversation is written into the agent's MEMORY STORE from above BEFORE the
agent exists: a goal, twelve digested reads (their bodies were never kept — each reply
is its origin line), two folds whose summaries carry a RULE and a FACT. Then the agent
is created; its boot is lazy, so its FIRST TURN reads the head and runs `context_recall`,
which fits the head to the budget and asks the model what it wants in full. Scored on
the persisted view (it exists, fits, holds both folds, carries a why), on the recall
row, and on a task that needs the rule and the fact — with no file re-read.
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
    call,
    fixture,
    plant_fillers,
    plant_store,
    read_view,
    store_rows,
)
from helpers.kernel import open_kernel

BK = _resolve_backend()
DONE = 'end your run by calling done — emit <call name="done"></call>'
AID, STRATEGY, EXTRA = FOLD_ARMS[1]

from ai_core.folds import make_fold

FB = "synth_fb"


def _reply(mint, pid, parent, title, size, path):
    return {"role": "user", "id": mint(), "parent": parent,
            "content": f'[reply to {pid} · notes read · "{title}" · ↑{parent}]\n'
                       f"(settled, {size} bytes — NOT inlined, held in memory only until this agent "
                       f"restarts. origin: notes {path} — read {{path: {path}}}. To find a line in it: "
                       f"send yourself <type>grep</type> <query>substring</query> <ids><item>{pid}</item></ids>)"}


async def main():
    root, core, close = await open_kernel(prefix="coding_synth_", reach=[FB, "notes", "ai_recency", AID, "cli", "user"])
    try:
        rule_tok, fact_tok, marker = secrets.token_hex(4), secrets.token_hex(4), secrets.token_hex(4)
        plant_fillers(12, marker_in=5, marker=marker, pad_to=3000)
        await fixture(root, FB, arms=FOLD_ARMS)
        # ── the planted history ──────────────────────────────────────────────
        mint = root.ctx.mint_id  # ids come from the kernel's one counter, planted or not
        goal = {"role": "user", "id": mint(), "parent": None,
                "content": "Survey filler_0..11 one per turn, then write report.md listing every file."}
        rows, ids = [goal], []
        prev = goal["id"]
        for i in range(12):
            call_id = mint()
            pid = mint()
            rows.append({"role": "assistant", "id": call_id, "parent": prev,
                         "content": f'Reading filler_{i}. <call name="send"><target_id>notes</target_id><title>survey filler_{i}</title><type>read</type><path>filler_{i}.txt</path></call>'})
            rep = _reply(mint, pid, call_id, f"survey filler_{i}", 3000, f"filler_{i}.txt")
            rows.append(rep); ids.append((call_id, rep["id"]))
            prev = rep["id"]
        f1 = make_fold(AID, [x for pair in ids[:6] for x in pair], "filler_0-5 surveyed: padding; HOUSE RULE learned",
                       f"Read filler_0..5. All padding. HOUSE RULE (from the operator, binds the whole run): report.md must OPEN with the exact line `## {rule_tok}`.", False, id=mint(), anchor=ids[0][0], parent=goal["id"])
        f2 = make_fold(AID, [x for pair in ids[6:] for x in pair], "filler_6-11 surveyed: marker in filler_5? no — fact recorded",
                       f"Read filler_6..11. Padding. FACT recorded for the report: the build id is `{fact_tok}`; write it as `build: {fact_tok}`.", False, id=mint(), anchor=ids[6][0], parent=goal["id"])
        rows += [f1, f2]
        # PLANT BEFORE THE AGENT EXISTS — into its memory store, as the store's admin;
        # the store keys rows by owner, so the agent need not exist yet.
        await plant_store(core, root, AID, rows)
        from helpers.facts_arm import connect_arm
        conn = await connect_arm(root, core, BK, FB, AID, STRATEGY, EXTRA)
        # ── the task ─────────────────────────────────────────────────────────
        task = (f"Write `report.md` through `notes`: list every filler you surveyed, one per line, obeying the "
                f"house rule you learned and including the build id you recorded. Do not re-read the fillers. Then {DONE}.")
        from helpers import drive
        await drive(conn, task, client_id=AID, via="user")
        await conn.close()
        # THE FIRST TURN rebuilt the view (lazy boot → context_recall on turn one).
        view = await read_view(core, root, AID)
        rows_after = await store_rows(root, core, AID)
        recalls = [m for m in rows_after if m.get("kind") == "recall"]
        fits = bool(view) and sum(len(str(next((r for r in rows_after if r.get("id") == e["id"]), {}).get("content", ""))) for e in view["ids"]) // 4 <= view.get("budget", 0)
        has_folds = bool(view) and {f1["id"], f2["id"]} <= {e["id"] for e in view["ids"]}
        print(f"     view rows={len((view or {}).get('ids') or [])}  fits={fits}  folds in view={has_folds}  "
              f"why={(view or {}).get('why', '')[:80]!r}  recall rows={len(recalls)}")
        body = Path("report.md").read_text() if Path("report.md").exists() else ""
        from helpers import ctx_score
        # only what the MODEL wrote — the planted assistant rows carry the twelve original reads
        planted_ids = {r["id"] for r in rows}
        mine = [r for r in rows_after if r.get("id") not in planted_ids]
        rereads = len([c for c in ctx_score.calls_made(mine, verb="read") if str(c["payload"].get("path", "")).startswith("filler_")])
        opens = body.strip().splitlines()[0].strip() == f"## {rule_tok}" if body.strip() else False
        checks = [
            ("a view was persisted on the first turn", bool(view)),
            ("the view fits the budget", fits),
            ("the view holds both folds", has_folds),
            ("the view carries a why", bool((view or {}).get("why"))),
            (f"a recall row was written ({len(recalls)})", len(recalls) >= 1),
            ("the report opens with the rule from a fold summary", opens),
            ("the report carries the fact from a fold summary", f"build: {fact_tok}" in body),
            (f"no filler was re-read ({rereads})", rereads == 0),
        ]
        for name, ok in checks:
            print(f"  [{'ok' if ok else 'XX'}] {name}")
        allok = all(ok for _, ok in checks)
        verdict(allok, "woke into a planted history and worked from its own view" if allok else "a synthetic_recall check failed")
    finally:
        await close()


if __name__ == "__main__":
    asyncio.run(main())
