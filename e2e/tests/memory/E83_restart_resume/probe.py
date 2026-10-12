"""memory/restart_resume — no amnesia between sessions.

Mid-task (five of ten reads done) the kernel is STOPPED and a new one starts on the
same directory, the memory store carried across. Told only to continue, the agent
finishes the SAME task — the report lists all ten — without re-reading a file it had
already read."""
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

from dataclasses import replace as _replace  # noqa: E402

N = 10


async def main():
    root, core, close = await open_kernel(prefix="mem_resume_", reach=["work_fb", "notes", "ai", "cli", "user"])
    try:
        plant_fillers(N, pad_to=3000)
        await M.files(root, ["ai"])
        conn = await M.agent(root, core, BK, "ai")
        conn.limits = _replace(conn.limits, max_turns=6)
        task = (f"Read `filler_0.txt` … `filler_{N - 1}.txt` through `notes`, ONE PER TURN, in order. Then write "
                f"`report.md` through `notes` listing every file you read, one per line. Then {DONE}")
        await drive(conn, task, client_id="ai", via="user")
        await conn.close()
        life1 = await store_rows(root, core, "ai")
        done1 = M.seen(life1)  # reads whose reply reached the model
        issued1 = set(M.reads(life1))
        root2, stop = await restart(root, core)
        before = await store_rows(root2, core, "ai")
        await M.ask(root2, "ai", "The kernel was restarted. Continue your task from where you stopped — do not "
                                 "redo what is already done.")
        after = await store_rows(root2, core, "ai")
        again = [p for p in M.reads(after[len(before):]) if p in done1]
        p = Path("report.md")
        body = p.read_text() if p.exists() else ""
        listed = sum(f"filler_{i}.txt" in body for i in range(N))
        print(f"     issued before the stop={sorted(issued1)}  answered={sorted(done1)}  re-read after={again}  listed={listed}/{N}  stop={stop}")
        ok = M.report([
            ("the first life stopped mid-task", 0 < len(done1) < N),
            (f"the report lists every file ({listed}/{N})", listed == N),
            (f"no file ANSWERED before the stop was read again ({again})", not again),
        ])
        verdict(ok, "resumed the same task in a new kernel without re-reading" if ok else "a restart_resume check failed")
    finally:
        await close()


if __name__ == "__main__":
    asyncio.run(main())
