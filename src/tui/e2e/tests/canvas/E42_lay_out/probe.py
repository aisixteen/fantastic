"""E42 lay_out — move cells spatially: slides into one row, in order."""

from __future__ import annotations

import asyncio
import re
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[2]))
from _lib.canvas_live import Rig, verdict, answer, ask, html_cell, http_get, in_frame, say, spot, wait_quiet  # noqa: E402,F401

def main() -> bool:
    rig = Rig(__file__)
    why = rig.plug_or_refusal()
    if why:
        return verdict(*rig.finish(False, why))
    rig.build()
    rig.make_home(rig.plug.record())

    spots = [(1300, 600), (760, 120), (1120, 380), (820, 700), (1500, 200), (980, 520)]
    order = [4, 1, 6, 2, 5, 3]
    cells = [html_cell(f"s{n}", f"Slide {n}", x, y, f"<h1>Slide {n}</h1>", 260, 180) for n, (x, y) in zip(order, spots)]
    rig.init(cells=cells)

    async def drive():
        async with rig.page(1800, 1000) as pg:
            r = await ask(pg, 250, 300, "Put the six slides in one row, in order, left to right.",
                          rig.budget(rig.limits["total_seconds"]), rig.limits["quiet_seconds"])
            await asyncio.sleep(2)  # the loader flushes
            return r
    try:
        done, said = asyncio.run(drive())
    finally:
        rig.stop()
    rects = rig.rects()
    pos = {c["id"]: (rects.get(c["id"], {}).get("x", 0), rects.get(c["id"], {}).get("y", 0)) for c in rig.cells() if c["id"].startswith("s")}
    xs = [pos[f"s{n}"][0] for n in range(1, 7)]
    ys = [pos[f"s{n}"][1] for n in range(1, 7)]
    in_order = all(a < b for a, b in zip(xs, xs[1:]))
    one_row = max(ys) - min(ys) <= 180
    rig.summary.update({"answer": said, "positions": pos})
    print(f"positions after: {pos}")
    return verdict(*rig.finish(in_order and one_row, f"x strictly increasing 1..6: {in_order} · one row (dy {max(ys)-min(ys)}): {one_row}"))


if __name__ == "__main__":
    sys.exit(0 if main() else 1)
