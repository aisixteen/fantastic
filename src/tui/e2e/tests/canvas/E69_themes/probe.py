"""E69 themes — the same cells in two themes (owner 2026-10-11): a phone-wide
tab is `mobile` by `auto` (under 768px) and stacks the canvas's cells in ONE
column; a desktop tab keeps the canvas rects; the cell records carry no geometry."""

from __future__ import annotations

import asyncio
import json
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[2]))
from _lib.canvas_live import GEOMETRY, Rig, html_cell, verdict  # noqa: E402

COLUMN_X, COLUMN_W = 0, 400


async def left_top(pg, cell: str) -> list:
    return await pg.js(f"(()=>{{const e=document.querySelector('.agent-frame[data-id={cell}]'); return e ? [parseFloat(e.style.left), parseFloat(e.style.top), parseFloat(e.style.width)] : null}})()") or []


def main() -> bool:
    rig = Rig(__file__, live=False)
    rig.build()
    rig.make_home(None)
    r: dict = {}
    info: dict = {}
    try:
        rig.init(cells=[html_cell("a", "A", 900, 100, "<p>A</p>", 340, 240), html_cell("b", "B", 100, 500, "<p>B</p>", 300, 200)])

        async def phone() -> None:
            async with rig.page(390, 800) as pg:
                await pg.until("document.querySelectorAll('.agent-frame').length === 2", 20)
                await asyncio.sleep(2.0)  # the loader flushes the placed rects
                info["phone_a"], info["phone_b"] = await left_top(pg, "a"), await left_top(pg, "b")
                # the pin: B held on the screen's bottom, still there after the column scrolls
                body = await pg.rect(".agent-frame[data-id=b] .frame-body")
                if body:  # the head's chrome shows while the cell is pointed at
                    await pg.cdp("Input.dispatchMouseEvent", type="mouseMoved", x=body["x"] + 20, y=body["y"] + 20)
                    await asyncio.sleep(0.3)
                pin = await pg.rect(".agent-frame[data-id=b] .frame-head .pin")
                info["pin_chip"] = pin
                if pin:
                    await pg.cdp("Input.dispatchMouseEvent", type="mouseMoved", x=pin["x"] + pin["w"] / 2, y=pin["y"] + pin["h"] / 2)
                    await pg.mouse("mousePressed", pin["x"] + pin["w"] / 2, pin["y"] + pin["h"] / 2)
                    await pg.mouse("mouseReleased", pin["x"] + pin["w"] / 2, pin["y"] + pin["h"] / 2)
                await pg.until("document.querySelector('.agent-frame[data-id=b]')?.classList.contains('pinned')", 10)
                bottom = "(()=>{const r=document.querySelector('.agent-frame[data-id=b]').getBoundingClientRect(); return [Math.round(r.left), Math.round(r.bottom), Math.round(r.width)]})()"
                info["pinned_at"] = await pg.js(bottom)
                # the canvas as drawn (a headless window is at least ~500px wide)
                info["canvas_box"] = await pg.js("(()=>{const r=document.getElementById('canvas').getBoundingClientRect(); return [Math.round(r.width), Math.round(r.bottom)]})()")
                # the column fills the width (a 12px gutter each side) and its chrome shows without hover
                info["a_screen_w"] = await pg.js("Math.round(document.querySelector('.agent-frame[data-id=a]').getBoundingClientRect().width)")
                info["a_head_opacity"] = await pg.js("getComputedStyle(document.querySelector('.agent-frame[data-id=a] > .frame-head')).opacity")
                await pg.cdp("Input.dispatchMouseEvent", type="mouseWheel", x=info["canvas_box"][0] - 4, y=300, deltaX=0, deltaY=300)  # the gutter: an iframe takes its own wheel
                await asyncio.sleep(0.6)
                info["pinned_after_scroll"] = await pg.js(bottom)
                info["phone_a_after_scroll"] = await left_top(pg, "a")
                info["a_top_after_scroll"] = await pg.js("document.querySelector('.agent-frame[data-id=a]').getBoundingClientRect().top")

        async def desktop() -> None:
            async with rig.page(1500, 950) as pg:
                await pg.until("document.querySelectorAll('.agent-frame').length === 2", 20)
                await asyncio.sleep(1.0)
                info["desk_a"] = await left_top(pg, "a")

        asyncio.run(phone())
        mobile = rig.rects("mobile")
        info["mobile_rects"] = mobile
        a, b = mobile.get("a", {}), mobile.get("b", {})
        r["mobile_layout_made"] = bool(a) and bool(b)
        r["one_column"] = all(x.get("x") == COLUMN_X and x.get("width") == COLUMN_W for x in (a, b))
        r["stacked_no_overlap"] = bool(a) and bool(b) and (a["y"] + a["height"] <= b["y"] or b["y"] + b["height"] <= a["y"])
        r["drawn_in_column"] = info["phone_a"][:1] == [COLUMN_X] and info["phone_b"][:1] == [COLUMN_X]
        pa, ps = info.get("pinned_at") or [], info.get("pinned_after_scroll") or []
        cw, cb = info.get("canvas_box") or [0, 0]
        r["pinned_on_screen_bottom"] = len(pa) == 3 and pa[0] == 8 and abs(pa[1] - (cb - 8)) <= 2 and abs(pa[2] - (cw - 16)) <= 1
        r["column_full_width"] = isinstance(info.get("a_screen_w"), (int, float)) and abs(info["a_screen_w"] - (cw - 24)) <= 2
        r["chrome_always_shown"] = info.get("a_head_opacity") == "1"
        r["column_scrolled"] = isinstance(info.get("a_top_after_scroll"), (int, float)) and info["a_top_after_scroll"] < 0
        r["pin_holds_while_scrolling"] = ps == pa
        r["pin_saved_in_layout"] = rig.pinned("mobile") == "b"
        asyncio.run(desktop())
        r["canvas_kept"] = info["desk_a"][:2] == [900, 100] and rig.rects("canvas").get("a", {}).get("x") == 900
        r["cells_carry_no_geometry"] = all(not any(k in c for k in GEOMETRY) for c in rig.cells())
    finally:
        rig.stop()
    print(json.dumps(info, indent=1))
    ok = bool(r) and all(r.values())
    rig.summary["checks"] = r
    rig.summary["info"] = info
    failed = [k for k, v in r.items() if not v]
    return verdict(*rig.finish(ok, "all checks pass" if ok else f"failed: {', '.join(failed)}"))


if __name__ == "__main__":
    sys.exit(0 if main() else 1)
