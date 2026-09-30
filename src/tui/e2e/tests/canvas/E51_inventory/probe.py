"""E51 inventory — self-description of a big canvas (dara-sized)."""

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

    cells = []
    i = 0
    def at():
        global_i = len(cells)
        return 700 + 300 * (global_i % 6), 40 + 220 * (global_i // 6)
    for n in range(14):
        cells.append(html_cell(f"flow_{n}", f"Flow {chr(65 + n)}", *at(), f"<h2>Flow {chr(65 + n)}</h2>", 280, 190))
    for n in range(6):
        cells.append(html_cell(f"doc_{n}", f"Doc {n + 1}", *at(), f"<embed src='doc{n}.pdf' type='application/pdf'>", 280, 190))
    for n in range(4):
        cells.append(html_cell(f"svc_{n}", f"Service :{3001 + n}", *at(), f"<iframe src='http://127.0.0.1:{3001 + n}/'></iframe>", 280, 190))
    for n in range(3):
        x, y = at()
        cells.append({"id": f"gl_{n}", "handler_module": "gl_agent.ts", "display_name": f"Scene {n + 1}", "x": x, "y": y,
                      "width": 280, "height": 190, "gl_source": "scene.add(new THREE.Mesh(new THREE.PlaneGeometry(1,1)));"})
    rig.init(cells=cells)
    want = {"html": 24, "gl": 3}

    async def drive():
        async with rig.page(1900, 1100) as pg:
            return await ask(pg, 250, 300, "Give me an inventory of this canvas: how many cells of each kind are there?",
                             rig.limits["total_seconds"], rig.limits["quiet_seconds"])
    try:
        done, said = asyncio.run(drive())
    finally:
        rig.stop()
    calls = rig.page_calls()
    rig.summary.update({"answer": said, "page_calls": len(calls), "want": want})
    print(f"page calls: {len(calls)} · answer: {said[:500]}")
    n = lambda k: bool(re.search(rf"(?<!\d){k}(?!\d)", said))
    # "each kind" read two ways, both exact: by HANDLER (24 html + 3 GL) or by
    # ROLE (14 flows · 6 docs · 4 services · 3 GL)
    counts = (n(24) and n(3)) or (n(14) and n(6) and n(4) and n(3))
    bounded = len(calls) <= rig.limits["max_page_calls"]
    return verdict(*rig.finish(counts and bounded, f"counts exact (by handler 24+3, or by role 14/6/4/3): {counts} · {len(calls)} page calls (≤ {rig.limits['max_page_calls']}): {bounded}"))


if __name__ == "__main__":
    sys.exit(0 if main() else 1)
