"""memory/budget_trigger — the effective-context ceiling.

A survey fills a squeezed window. Compaction must fire EARLY — at 0.8 of the budget,
measured on the endpoint's own reported prompt size (calibration) — and the run must
never hit `too_small`."""
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
    root, core, close = await open_kernel(prefix="mem_trigger_", reach=["work_fb", "notes", "ai", "cli", "user"])
    try:
        plant_fillers(N, pad_to=4000)
        await M.files(root, ["ai"])
        conn = await M.agent(root, core, BK, "ai")
        await drive(conn, f"Read `filler_0.txt` … `filler_{N - 1}.txt` through `notes`, ONE PER TURN. Do not write "
                          f"anything. After the last one, {DONE}", client_id="ai", via="user")
        await conn.close()
        st = await M.status(root, "ai")
        too_small = [t for t in conn.trips if "too_small" in t]
        reported = st.get("reported_prompt_tokens")
        print(f"     compactions={len(conn.compactions)} trigger={st.get('trigger')} calibration={st.get('calibration')} "
              f"reported={st.get('reported_prompt_tokens')} too_small={too_small}")
        ok = M.report([
            (f"compaction fired ({len(conn.compactions)})", len(conn.compactions) >= 1),
            ("the trigger is 0.8 of the budget", st.get("trigger") == 0.8),
            # The calibration follows WHAT THE ENDPOINT REPORTS: a reported prompt size moves
            # it off 1.0; an endpoint that streams no usage leaves it at 1.0.
            (f"calibration follows the endpoint ({'reported' if reported else 'no usage streamed'})",
             (bool(reported) and st.get("calibration") != 1.0) or (not reported and st.get("calibration") == 1.0)),
            ("never too_small", not too_small),
        ])
        verdict(ok, "fired at 0.8 of the budget; calibration follows the endpoint; never too_small" if ok else "a budget_trigger check failed")
    finally:
        await close()


if __name__ == "__main__":
    asyncio.run(main())
