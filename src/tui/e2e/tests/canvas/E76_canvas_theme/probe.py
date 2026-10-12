"""E76 canvas theme — themes like backgrounds (owner 2026-10-12): see README."""

from __future__ import annotations

import asyncio
import json
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[2]))
from _lib.canvas_live import Rig, html_cell, spot, verdict  # noqa: E402

ROWS = "[...document.querySelectorAll('.bgs .row .name b')].map(b=>b.textContent)"
COLUMN = "document.getElementById('canvas').classList.contains('theme-mobile')"


def main() -> bool:
    rig = Rig(__file__, live=False)
    rig.build()
    rig.make_home(None)
    r: dict = {}
    info: dict = {}
    try:
        rig.init(cells=[html_cell("a", "A", 100, 100, "<p>A</p>", 300, 200)])

        async def run() -> None:
            async with rig.page(1400, 900) as pg:
                await pg.until("document.querySelectorAll('.agent-frame').length === 1", 20)
                await spot(pg, 800, 300, "canvas theme")
                await pg.until("!!document.querySelector('.bgs h2')", 15)
                await pg.until("[...document.querySelectorAll('.bgs h2')].length >= 2", 10)
                info["headings"] = await pg.js("[...document.querySelectorAll('.bgs h2')].map(h=>h.firstChild.textContent)")
                info["rows"] = await pg.js(ROWS)
                info["column_at_start"] = await pg.js(COLUMN)
                # + new theme: tall, one column, auto from 1000px
                await pg.js("[...document.querySelectorAll('.bgs .btn')].find(b=>b.textContent==='+ new theme').click()")
                await pg.until("!!document.querySelector('.bgs .tform')", 5)
                await pg.js("""(()=>{const f=document.querySelector('.bgs .tform'); const [name, note]=[...f.querySelectorAll('input:not([type=number])')]; name.value='tall';
                  const [layout, kind]=f.querySelectorAll('select'); layout.value='column'; kind.value='min'; f.querySelector('input[type=number]').value='1000';
                  [...f.querySelectorAll('button')].find(b=>b.textContent==='save').click();})()""")
                info["tall_listed"] = bool(await pg.until(f"{ROWS}.includes('tall')", 8))
                info["auto_picks_tall"] = bool(await pg.until(COLUMN, 8))
                # use canvas by hand
                await pg.js("[...document.querySelectorAll('.bgs .row .name')].find(b=>b.querySelector('b').textContent==='canvas').click()")
                info["canvas_by_hand"] = bool(await pg.until(f"!{COLUMN}", 8))
                # hide stock mobile, restore it
                await pg.js("[...document.querySelectorAll('.bgs .row')].find(r=>r.querySelector('.name b')?.textContent==='mobile')?.querySelectorAll('.x')[1].click()")
                info["mobile_hidden"] = bool(await pg.until(f"!{ROWS}.includes('mobile')", 8))
                await pg.js("[...document.querySelectorAll('.bgs .link')].find(b=>b.textContent.startsWith('restore stock') && b.closest('h2')?.firstChild.textContent==='Themes')?.click()")
                info["mobile_restored"] = bool(await pg.until(f"{ROWS}.includes('mobile')", 8))
                info["console"] = [x for x in pg.logs if "WebGL" not in x][:6]

        asyncio.run(run())
        r["two sections: Backgrounds and Themes"] = info.get("headings", [])[:2] == ["Backgrounds", "Themes"]
        r["stock themes listed"] = {"canvas", "mobile"} <= set(info.get("rows") or [])
        r["a wide tab starts on the free canvas"] = info.get("column_at_start") is False
        r["a theme added through the form is listed"] = info.get("tall_listed") is True
        r["auto picks it by its trigger"] = info.get("auto_picks_tall") is True
        r["a theme used by hand holds"] = info.get("canvas_by_hand") is True
        r["a stock theme hides and restores"] = info.get("mobile_hidden") is True and info.get("mobile_restored") is True
    finally:
        rig.stop()
    rig.summary["checks"] = r
    rig.summary["info"] = info
    print(json.dumps(info, indent=1, default=str)[:2000])
    failed = [k for k, v in r.items() if not v]
    return verdict(*rig.finish(not failed, f"{len(r) - len(failed)}/{len(r)} pass" + ("" if not failed else f" — failed: {'; '.join(failed)}")))


if __name__ == "__main__":
    sys.exit(0 if main() else 1)
