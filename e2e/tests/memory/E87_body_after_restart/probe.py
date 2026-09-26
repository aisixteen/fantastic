"""memory/body_after_restart — bodies are never persisted.

A large file is read (its reply digested — held in RAM only, its origin line in the
store). The kernel restarts; the body is gone. Asked for a line deep in the file, the
agent either re-reads it through its origin or had written the value down before —
and its answer is right. This also measures life without `pull` (it no longer exists)."""
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


async def main():
    root, core, close = await open_kernel(prefix="mem_body_", reach=["work_fb", "notes", "ai", "cli", "user"])
    try:
        code = secrets.token_hex(4)
        # ONE PAGE (a `read` returns 200 lines): the code is in what the first read
        # returned, and long lines keep that reply over the inline cap, so it is
        # digested — its body in RAM only, gone after the restart.
        # labels ARE the line numbers (row N on line N), so "row 137" means one thing
        lines = [f"row {i + 1}: {secrets.token_hex(16)}" for i in range(180)]
        lines[136] = f"row 137: the vault code is {code}"
        Path("ledger.txt").write_text("\n".join(lines) + "\n")
        await M.files(root, ["ai"])
        conn = await M.agent(root, core, BK, "ai")
        await drive(conn, f"Read `ledger.txt` through `notes`. Do NOT write anything down about "
                          f"row 137. Then {DONE}", client_id="ai", via="user")
        await conn.close()
        root2, _ = await restart(root, core)
        before = await store_rows(root2, core, "ai")
        digested = any("NOT inlined" in str(r.get("content")) and "origin:" in str(r.get("content")) for r in before)
        await M.ask(root2, "ai", f"What is the vault code on row 137 of `ledger.txt`? Answer `CODE <value>`, then {DONE}")
        after = await store_rows(root2, core, "ai")
        ans = M.answers(after, len(before))
        reread = len(M.reads(after[len(before):], "ledger.txt"))
        from helpers import ctx_score

        greps = len(ctx_score.calls_made(after[len(before):], target="ai", verb="grep"))
        print(f"     digested before={digested}  re-reads after restart={reread}  greps={greps}  answer={ans[-160:]!r}")
        ok = M.report([
            ("the read was digested — its body never stored", digested),
            ("the answer is right", code in ans),
            ("it re-read the source after the restart (the body was gone)", reread >= 1),
        ])
        verdict(ok, "no body survived the restart; the model re-read its origin" if ok else "a body_after_restart check failed")
    finally:
        await close()


if __name__ == "__main__":
    asyncio.run(main())
