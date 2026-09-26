"""memory/why_survives — compaction keeps the WHY, not only the decision.

A decision and its REASON are told at turn 1; a survey of twelve fillers at a squeezed
window makes the conversation fold; asked afterwards (no re-reads), the agent states
the decision AND the reason."""
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
    root, core, close = await open_kernel(prefix="mem_why_", reach=["work_fb", "notes", "ai", "cli", "user"])
    try:
        port, why = 1000 + secrets.randbelow(9000), secrets.token_hex(3)
        plant_fillers(N, pad_to=4000)
        await M.files(root, ["ai"])
        conn = await M.agent(root, core, BK, "ai")
        await drive(conn, f"A decision for this whole run: we deploy on port {port}. The REASON — keep it, it "
                          f"matters later: the lab firewall `{why}` admits no other port. Acknowledge the "
                          f"decision and its reason in one sentence, then {DONE}", client_id="ai", via="user")
        await M.ask(root, "ai", f"Read `filler_0.txt` … `filler_{N - 1}.txt` through `notes`, ONE PER TURN, noting "
                                f"anything unusual. Do not write anything. After the last one, {DONE}")
        before = await store_rows(root, core, "ai")
        await M.ask(root, "ai", "Without reading anything again: which port did we decide, and WHY? Answer "
                                f"exactly `PORT <n> BECAUSE <reason>`, then {DONE}")
        await conn.close()
        ans = M.answers(await store_rows(root, core, "ai"), len(before))
        print(f"     compactions={len(conn.compactions)}  answer={ans[-240:]!r}")
        ok = M.report([
            (f"the conversation was compacted ({len(conn.compactions)})", len(conn.compactions) >= 1),
            ("the decision survived", str(port) in ans),
            ("the REASON survived, not just the decision", why in ans),
        ])
        verdict(ok, "decision and reason both survived compaction" if ok else "a why_survives check failed")
    finally:
        await close()


if __name__ == "__main__":
    asyncio.run(main())
