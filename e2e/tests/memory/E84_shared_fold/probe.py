"""memory/shared_fold — knowledge that is not a silo.

Agent A surveys twelve fillers at a squeezed window (its reads fold); one filler holds
an audit marker. A is asked to SHARE the fold that covers it with B — an explicit choice
of A's model, recorded as a grant in the store. B starts from the grant (`fork_from`)
and must answer the marker — a member detail the fold summary need not carry."""
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

N = 12


async def main():
    root, core, close = await open_kernel(prefix="mem_sfold_", reach=["work_fb", "notes", "ai_a", "ai_b", "cli", "user"])
    try:
        marker = secrets.token_hex(4)
        plant_fillers(N, marker_in=3, marker=marker, pad_to=4000)
        await M.files(root, ["ai_a", "ai_b"])
        a = await M.agent(root, core, BK, "ai_a", record={"context_async": False})
        await drive(a, f"Read `filler_0.txt` … `filler_{N - 1}.txt` through `notes`, ONE PER TURN, noting anything "
                       f"unusual. Do not write anything. After the last one, {DONE}", client_id="ai_a", via="user")
        await M.ask(root, "ai_a", "Agent `ai_b` needs what you found in `filler_3.txt`. Share with it the FOLD of "
                                  "yours that covers that read: send your memory store `memory` "
                                  "<type>share</type> <with>ai_b</with> <scope><fold>ID</fold></scope> <why>…</why> "
                                  "— your fold rows show their ids as `[fold <id> …]`. If none covers it, share "
                                  f"<scope><head_until>ID</head_until></scope> with your newest row id instead. Then {DONE}")
        await a.close()
        tables = await memory.dump(root)
        grants = [g for g in tables.get("grants") or [] if str(g.get("owner", "")).endswith("|ai_a")]
        b = await M.agent(root, core, BK, "ai_b")
        out = await M.call(root, "ai_b", {"type": "fork_from", "owner": "ai_a",
                                          "grant": grants[0]["grant"] if grants else None, "client_id": "ai_b",
                                          "message": "Which audit marker did ai_a find in `filler_3.txt`? The fold "
                                                     "summary may not say — look inside. Answer `MARKER <value>`, "
                                                     f"then {DONE}"})
        for _ in range(180):
            await asyncio.sleep(1.0)
            if any(r.get("role") == "assistant" for r in await store_rows(root, core, "ai_b")):
                break
        await M.ask(root, "ai_b", "If you have not answered yet, answer now: `MARKER <value>`.", timeout=120)
        await b.close()
        ans = M.answers(await store_rows(root, core, "ai_b"))
        print(f"     grants={[g.get('scope') for g in grants]}  fork={out}  answer={ans[-200:]!r}")
        ok = M.report([
            ("A's model shared by its own choice (a grant exists)", bool(grants)),
            ("B started from the grant", isinstance(out, dict) and out.get("forked") is True),
            ("B answered the member detail", marker in ans),
        ])
        verdict(ok, "a shared fold carried the finding to another agent" if ok else "a shared_fold check failed")
    finally:
        await close()


if __name__ == "__main__":
    asyncio.run(main())
