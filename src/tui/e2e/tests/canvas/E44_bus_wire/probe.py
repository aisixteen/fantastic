"""E44 bus_wire — per-pair emit grants, as dawee's instruments → mixer."""

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

    def source(cid, name, x, y):
        return html_cell(cid, name, x, y, f"<h2>{name}</h2><button id='hit' onclick=\"fantastic.emit('mixer',{{type:'hit',from:'{cid}'}})\">hit</button>", 220, 150)
    mixer = html_cell("mixer", "Mixer", 1150, 420,
        "<h2>Mixer</h2><pre id='c'>{}</pre><script>const c={};fantastic.onMessage(p=>{if(p&&p.type==='hit'){c[p.from]=(c[p.from]||0)+1;document.getElementById('c').textContent=JSON.stringify(c)}})</script>", 300, 220)
    rig.init(cells=[source("kick", "Kick", 700, 80), source("snare", "Snare", 950, 80), source("hat", "Hat", 1200, 80),
                    source("noise", "Noise", 1450, 80), mixer])

    async def drive():
        async with rig.page(1800, 1000) as pg:
            done, said = await ask(pg, 250, 300,
                "Wire Kick, Snare and Hat to the Mixer so their hits reach it. Leave Noise unwired.",
                rig.limits["total_seconds"], rig.limits["quiet_seconds"])
            await asyncio.sleep(2)
            for cid in ("kick", "snare", "hat", "noise"):
                await in_frame(pg, cid, "(doc.getElementById('hit')?.click(), true)")
                await asyncio.sleep(0.5)
            await asyncio.sleep(1.5)
            counts = await in_frame(pg, "mixer", "doc.getElementById('c').textContent")
            return said, counts
    try:
        said, counts = asyncio.run(drive())
    finally:
        rig.stop()
    import json
    try:
        c = json.loads(counts or "{}")
    except Exception:
        c = {}
    rig.summary.update({"answer": said, "mixer_counts": c})
    print(f"mixer counts after one hit each: {c}")
    ok = all(c.get(k, 0) >= 1 for k in ("kick", "snare", "hat")) and c.get("noise", 0) == 0
    return verdict(*rig.finish(ok, f"kick/snare/hat reach the mixer, noise does not: {ok} ({c})"))


if __name__ == "__main__":
    sys.exit(0 if main() else 1)
