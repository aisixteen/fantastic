"""memory/repeat_read — duplicate large reads (python, in process).

The agent reads the same file three times. What the MODEL SEES next (the assembled
view) holds the body ONCE; the later reads are one-line stubs citing the first row."""
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
    root, core, close = await open_kernel(prefix="mem_repeat_", reach=["work_fb", "notes", "ai", "cli", "user"])
    try:
        body = "\n".join(f"line {i}: {secrets.token_hex(8)}" for i in range(60))
        Path("big.md").write_text(body + "\n")
        await M.files(root, ["ai"])
        conn = await M.agent(root, core, BK, "ai", record={"context_async": False})
        await drive(conn, f"Read `big.md` through `notes` THREE times, one read per turn (it is a test of the "
                          f"reader). Then {DONE}", client_id="ai", via="user")
        await conn.close()
        rows = await store_rows(root, core, "ai")
        n_reads = len(M.reads(rows, "big.md"))
        msgs, _ = await core._assemble("ai", "(probe)", root, "ai")
        seen = "\n".join(str(m.get("content")) for m in msgs)
        full = seen.count("line 59:")
        stubs = seen.count("[identical to the reply in row")
        print(f"     reads={n_reads}  full copies in view={full}  stubs={stubs}")
        ok = M.report([
            (f"the file was read at least twice ({n_reads})", n_reads >= 2),
            ("the view carries the body once", full == 1),
            ("the later reads are stubs citing the first", stubs >= n_reads - 1),
        ])
        verdict(ok, "one body in view; repeats cite the first" if ok else "a repeat_read check failed")
    finally:
        await close()


if __name__ == "__main__":
    asyncio.run(main())
