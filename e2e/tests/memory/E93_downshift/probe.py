"""memory/downshift — the window shrinks under a running agent.

The agent works at a large window (no compaction needed); then its record is updated
to a small window — as a model switch would. Its next turn is fitted to the new window:
compaction fires, and nothing is `too_small`."""
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

N = 8


async def main():
    root, core, close = await open_kernel(prefix="mem_down_", reach=["work_fb", "notes", "ai", "cli", "user"])
    try:
        plant_fillers(N, pad_to=4000)
        await M.files(root, ["ai"])
        conn = await M.agent(root, core, BK, "ai", window=64000, record={"context_async": False})
        await drive(conn, f"Read `filler_0.txt` … `filler_{N - 1}.txt` through `notes`, ONE PER TURN. Do not write "
                          f"anything. After the last one, {DONE}", client_id="ai", via="user")
        before = len(conn.compactions)
        r = await M.call(root, root.id, {"type": "update_agent", "id": "ai", "context_window": 12000, "num_ctx": 12000})
        await M.ask(root, "ai", f"In one sentence: what did the files have in common? Then {DONE}")
        await conn.close()
        st = await M.status(root, "ai")
        too_small = [t for t in conn.trips if "too_small" in t]
        print(f"     compactions before={before} after={len(conn.compactions)} window now={st.get('context_window')} "
              f"too_small={too_small} update={str(r)[:80]}")
        ok = M.report([
            ("no compaction at the large window", before == 0),
            ("the next turn after the switch compacted", len(conn.compactions) > before),
            ("never too_small", not too_small),
        ])
        verdict(ok, "fitted to the smaller window on the very next turn" if ok else "a downshift check failed")
    finally:
        await close()


if __name__ == "__main__":
    asyncio.run(main())
