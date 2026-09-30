"""E52 hydrate_scale — the dara-sized load regression (55 s → 5 s), at 300 cells."""

from __future__ import annotations

import asyncio
import re
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[2]))
from _lib.canvas_live import Rig, verdict, answer, ask, html_cell, http_get, in_frame, say, spot, wait_quiet  # noqa: E402,F401

def main() -> bool:
    import time
    rig = Rig(__file__, live=False)
    rig.build()
    rig.make_home(None)
    cells = [html_cell(f"c{n}", f"Cell {n}", 700 + 120 * (n % 20), 40 + 90 * (n // 20), f"<p>{n}</p>", 110, 80) for n in range(300)]
    rig.init(cells=cells)

    async def drive():
        async with rig.page(1900, 1100) as pg:
            t0 = time.time()
            ok = await pg.until("document.querySelectorAll('.agent-frame').length >= 300", rig.limits["render_seconds"])
            return ok, time.time() - t0, await pg.js("document.querySelectorAll('.agent-frame').length")
    try:
        ok, secs, n = asyncio.run(drive())
    finally:
        rig.stop()
    rig.summary.update({"frames": n, "seconds": round(secs, 1)})
    return verdict(*rig.finish(bool(ok), f"{n} frames in {secs:.1f} s (bound {rig.limits['render_seconds']} s)"))


if __name__ == "__main__":
    sys.exit(0 if main() else 1)
