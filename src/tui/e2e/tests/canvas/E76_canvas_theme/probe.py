"""E76 canvas theme — themes like backgrounds (owner 2026-10-12): see README."""

from __future__ import annotations

import asyncio
import json
import subprocess
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
                # no "+ new theme" (owner 2026-10-12): a new theme is the AI agent's —
                # `add_theme` on the agent, as an AI calls it: tall, one column, auto from 1000px
                info["no_new_theme_button"] = not await pg.js("[...document.querySelectorAll('.bgs .btn')].some(b=>b.textContent.includes('new theme'))")
                info["ask_ai_hint"] = await pg.js("[...document.querySelectorAll('.bgs .hint')].some(h=>h.textContent.includes('To make a new theme, ask your AI agent'))")
                agent = await pg.js("document.querySelector('.bgs').closest('.agent-frame').dataset.id")
                p = subprocess.run([str(rig.binary), "send", json.dumps({"target": f"{rig.page_guid()}|{agent}", "payload": {"type": "add_theme", "name": "tall", "layout": "column", "trigger": {"min_width": 1000}}})],
                                   cwd=rig.proj, env=rig.env(), capture_output=True, text=True, timeout=60)
                info["add_theme"] = p.stdout[-300:] or p.stderr[-300:]
                info["tall_listed"] = bool(await pg.until(f"{ROWS}.includes('tall')", 8))
                info["auto_picks_tall"] = bool(await pg.until(COLUMN, 8))
                # use canvas by hand
                await pg.js("[...document.querySelectorAll('.bgs .row .name')].find(b=>b.querySelector('b').textContent==='canvas').click()")
                info["canvas_by_hand"] = bool(await pg.until(f"!{COLUMN}", 8))
                # the default theme has no ×; a direct delete is refused
                info["canvas_no_x"] = await pg.js("[...document.querySelectorAll('.bgs .row')].find(r=>r.querySelector('.name b')?.textContent==='canvas')?.querySelectorAll('.x').length === 1")
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
        r["no + new theme; the panel says to ask the AI agent"] = info.get("no_new_theme_button") is True and info.get("ask_ai_hint") is True
        r["a theme added with add_theme (as an AI does) is listed"] = info.get("tall_listed") is True
        r["the default canvas theme has no ×"] = info.get("canvas_no_x") is True
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
