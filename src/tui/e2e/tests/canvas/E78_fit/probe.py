"""E78 fit — the view's `fit {id}` sizes an html cell to what it draws."""

from __future__ import annotations

import asyncio
import json
import subprocess
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[2]))
from _lib.canvas_live import Rig, html_cell, verdict  # noqa: E402

BOX = "<div style='width:640px;height:360px;background:#246'>box</div>"
TEXT = "<p style='margin:0;font:16px sans-serif'>" + "fit to content wraps this paragraph at the width it is given. " * 12 + "</p>"
# the frame's size on the page, and (a trusted panel: same-origin) whether its document still overflows
FRAME = "(id)=>{const f=document.querySelector('.agent-frame[data-id='+JSON.stringify(id)+']'); return f?[f.offsetWidth,f.offsetHeight]:null}"
OVER = ("(id)=>{const d=document.querySelector('.agent-frame[data-id='+JSON.stringify(id)+'] iframe')?.contentDocument?.documentElement;"
        " return d?[d.scrollWidth>d.clientWidth+1, d.scrollHeight>d.clientHeight+1]:null}")


def near(a: float, b: float, tol: float = 4) -> bool:
    return abs(a - b) <= tol


def main() -> bool:
    rig = Rig(__file__, live=False)
    rig.build()
    rig.make_home(None)
    r: dict = {}
    info: dict = {}
    try:
        rig.init(cells=[
            {**html_cell("sb", "Sandboxed box", 100, 100, BOX, 200, 120), "trusted": False},
            html_cell("tb", "Trusted box", 100, 600, BOX, 200, 120),
            html_cell("tx", "Text", 900, 100, TEXT, 200, 120),
        ])

        def send(target: str, payload: dict) -> dict:
            p = subprocess.run([str(rig.binary), "send", json.dumps({"target": target, "payload": payload})],
                               cwd=rig.proj, env=rig.env(), capture_output=True, text=True, timeout=60)
            try:
                return json.loads(p.stdout).get("result") or {}
            except ValueError:
                return {"error": (p.stdout + p.stderr)[-300:]}

        async def run() -> None:
            async with rig.page(1800, 1100) as pg:
                await pg.until("document.querySelectorAll('.agent-frame iframe').length === 3", 20)
                guid = ""
                for _ in range(100):
                    guid = rig.page_guid()
                    if guid:
                        break
                    await asyncio.sleep(0.1)
                view = f"{guid}|view_root"
                for cid in ("sb", "tb"):
                    rep = send(view, {"type": "fit", "id": cid})
                    info[f"{cid}_reply"] = rep
                    await asyncio.sleep(0.5)
                    fr = await pg.js(f"({FRAME})({json.dumps(cid)})") or [0, 0]
                    info[f"{cid}_frame"] = fr
                    # the box plus the panel body's default 8px margins
                    r[f"{cid}: fitted to the 640×360 box"] = near(rep.get("width", 0), 656) and near(rep.get("height", 0), 376) and rep.get("overflowed") is False
                    r[f"{cid}: the frame on the page is that size"] = near(fr[0], rep.get("width", -99)) and near(fr[1], rep.get("height", -99))
                info["tb_overflow"] = await pg.js(f"({OVER})('tb')")
                r["tb: its document no longer overflows"] = info["tb_overflow"] == [False, False]
                rep = send(view, {"type": "fit", "id": "tx", "max_width": 400})
                info["tx_reply"] = rep
                await asyncio.sleep(0.5)
                info["tx_overflow"] = await pg.js(f"({OVER})('tx')")
                r["tx: the paragraph wraps at the max width, its height fitted"] = rep.get("width") == 400 and rep.get("height", 0) > 120 and rep.get("overflowed") is False
                r["tx: nothing overflows at that size"] = info["tx_overflow"] == [False, False]
                r["fit refuses what is not its cell"] = send(view, {"type": "fit", "id": "nope"}).get("reason") == "not_found"
                # the layout holds it (a reload keeps it)
                rects = rig.rects()
                info["rects"] = {k: rects.get(k) for k in ("sb", "tb", "tx")}
                r["the layout holds the fitted rects"] = all(near((rects.get(k) or {}).get("width", 0), (info[f"{k}_reply"]).get("width", -99)) for k in ("sb", "tb", "tx"))

        asyncio.run(run())
    finally:
        rig.stop()
    rig.summary["checks"] = r
    rig.summary["info"] = info
    print(json.dumps(info, indent=1, default=str)[:2000])
    failed = [k for k, v in r.items() if not v]
    return verdict(*rig.finish(not failed, f"{len(r) - len(failed)}/{len(r)} pass" + ("" if not failed else f" — failed: {'; '.join(failed)}")))


if __name__ == "__main__":
    sys.exit(0 if main() else 1)
