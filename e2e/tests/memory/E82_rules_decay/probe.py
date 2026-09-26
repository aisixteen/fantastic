"""memory/rules_decay — constraint compliance under many turns.

Three rules about the final report are given at turn 1; twenty filler reads follow at
a squeezed window; then the report is written. Scored on the FOLD arm holding all three
rules; the TRUNCATE arm runs the same and is printed beside it as the control."""
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
from helpers import drive, memory, verdict  # noqa: F401
from helpers import mem_probe as M
from helpers.facts_arm import plant_fillers, store_rows
from helpers.kernel import open_kernel, restart  # noqa: F401

BK = _resolve_backend()
DONE = M.DONE

from helpers.facts_arm import rules_for, score_rules  # noqa: E402

N = 20


async def _arm(root, core, aid, strategy, rules, tok):
    conn = await M.agent(root, core, BK, aid, strategy=strategy)
    spec = "; ".join(f"{how} `{line}`" for how, line in rules)
    await drive(conn, f"Rules for the report you will write at the END of this task: {spec}. Acknowledge the "
                      f"rules, then {DONE}", client_id=aid, via="user")
    await M.ask(root, aid, f"Read `filler_0.txt` … `filler_{N - 1}.txt` through `notes`, ONE PER TURN. Do not write "
                           f"anything yet. After the last one, {DONE}")
    await M.ask(root, aid, f"Now write `{aid}.md` through `notes` listing the files you read, obeying every rule "
                           f"you were given at the start. Then {DONE}")
    await conn.close()
    p = Path(f"{aid}.md")
    for _ in range(40):
        if p.exists() and p.read_text().strip():
            break
        await asyncio.sleep(0.25)
    body = p.read_text() if p.exists() else ""
    held, lost = score_rules(body, rules)
    return held, lost, len(conn.compactions)


async def main():
    root, core, close = await open_kernel(prefix="mem_rules_", reach=["work_fb", "notes", "ai_trunc", "ai_fold", "cli", "user"])
    try:
        tok = {"first": secrets.token_hex(3), "last": secrets.token_hex(3), "owner": secrets.token_hex(3)}
        rules = rules_for(tok, ("owner",))
        plant_fillers(N, pad_to=3000)
        await M.files(root, ["ai_trunc", "ai_fold"])
        trunc = await _arm(root, core, "ai_trunc", "truncate", rules, tok)
        fold = await _arm(root, core, "ai_fold", "fold", rules, tok)
        print(f"     truncate: held {trunc[0]}/{len(rules)} lost={trunc[1]} cuts={trunc[2]}   "
              f"fold: held {fold[0]}/{len(rules)} lost={fold[1]} cuts={fold[2]}")
        ok = M.report([
            (f"the fold arm was compacted ({fold[2]})", fold[2] >= 1),
            (f"the fold arm held every rule ({fold[0]}/{len(rules)})", fold[0] == len(rules)),
        ])
        verdict(ok, "the rules held across twenty turns and the folds" if ok else "a rules_decay check failed")
    finally:
        await close()


if __name__ == "__main__":
    asyncio.run(main())
