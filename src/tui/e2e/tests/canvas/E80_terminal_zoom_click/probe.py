"""E80 terminal_zoom_click — a click in a terminal reaches the program as the
row and column under the pointer, whatever the canvas zoom (owner 2026-10-15:
in terminals the pointer did not point at the row; the canvas zooms with a CSS
`scale()` and xterm divided a scaled offset by its unscaled cell size):

a) a terminal cell runs CLICKS: alternate screen, SGR mouse reporting on, and
   every press it reads is logged as `<col> <row>` to `clicks.log`;
b) at zoom 1, zoomed in and zoomed out (the wheel on the background), the probe
   presses the centre of the grid cell (col 6, row 4) as the screen draws it;
c) each press reached CLICKS as `6 4`."""

from __future__ import annotations

import asyncio
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[2]))
from _lib.canvas_live import FOLDER, Rig, verdict, py_job  # noqa: E402
from helpers import oslayer  # noqa: E402

CLICKS = oslayer.RAW_TTY_PY + r'''import os, re, sys
sys.stdout.write("\x1b[?1049h\x1b[?1000h\x1b[?1006h\x1b[HREADY")
sys.stdout.flush()
buf = b""
while True:
    buf += os.read(0, 1024)
    for m in re.finditer(rb"\x1b\[<(\d+);(\d+);(\d+)M", buf):
        with open("clicks.log", "a") as log:
            log.write(f"{int(m[2])} {int(m[3])}\n")
    buf = buf[buf.rfind(b"M") + 1:] if b"M" in buf else buf
'''
CELL, BACKEND = "cell_clicks", "exec_bridge_clicks"
VERBS = ["watch", "status", "start", "write", "ack", "resize", "output"]
COL, ROW = 6, 4
FRAME = ".agent-frame[data-id=\"%s\"]" % CELL
# the grid as the screen draws it: the screen element's box, its scale, the grid size
GRID = ("(()=>{const f=document.querySelector('%s .frame-body'); const t=f&&f.xterm; const s=f&&f.querySelector('.xterm-screen');"
        " if(!t||!s||!s.offsetWidth) return null; const r=s.getBoundingClientRect();"
        " return {x:r.x,y:r.y,w:r.width,h:r.height,scale:r.width/s.offsetWidth,cols:t.cols,rows:t.rows};})()" % FRAME)
READY = "(()=>{const f=document.querySelector('%s .frame-body'); const t=f&&f.xterm; if(!t) return false;" \
        " const l=t.buffer.active.getLine(0); return !!l && l.translateToString(true).includes('READY');})()" % FRAME
# a point on the canvas background (no cell, no chrome under it), for the wheel
BG = ("(()=>{for(const [x,y] of [[innerWidth-30,innerHeight-30],[30,innerHeight-30],[innerWidth/2,innerHeight-30],[innerWidth-30,innerHeight/2]]){"
      " const e=document.elementFromPoint(x,y); if(e&&e.closest('#canvas')&&!e.closest('.agent-frame')) return [x,y];} return null;})()")


def main() -> bool:
    rig = Rig(__file__, live=False)
    rig.build()
    rig.make_home(None)
    rig.proj.mkdir(parents=True, exist_ok=True)
    (rig.proj / "clicks.py").write_text(CLICKS)
    cell = {"id": CELL, "handler_module": "terminal_view.ts", "backend_id": BACKEND, "owns_backend": False,
            "command": py_job("clicks.py")[0], "can_call": {"$self": "*", BACKEND: VERBS}, "callable_by": {"$self": "*"},
            "x": 500, "y": 300, "width": 420, "height": 300}
    host = {"id": BACKEND, "handler_module": "exec_bridge.tools", "cwd": ".", "inherit_env": ["*"], "shell": "login",
            "ingress_rule": "allow_all_ingress", "allowed": [py_job("clicks.py")[1]], "cell_id": CELL, "can_call": {"$self": "*"},
            "callable_by": {"$self": "*", "mcp": ["*"], "user": ["*"], "canvas_root": ["*"], "canvas_ws": ["watch"]}}
    rig.init(cells=[cell], host_agents=[(FOLDER, host)])
    log = rig.proj / "clicks.log"
    r: dict = {}
    info: dict = {}

    def clicks() -> list[str]:
        return log.read_text().splitlines() if log.is_file() else []

    async def zoom_until(pg, delta: float, ok) -> dict | None:
        bg = await pg.js(BG)
        info.setdefault("bg", bg)
        if not bg:
            return None
        for _ in range(40):
            g = await pg.js(GRID)
            if g and ok(g["scale"]):
                # the zoom's lerp at REST: the grid where two reads apart agree (a
                # loaded runner is still easing at a fixed 0.6 s, and a press there
                # misses the moving grid)
                last = None
                for _ in range(50):
                    await asyncio.sleep(0.15)
                    g = await pg.js(GRID)
                    if g == last:
                        break
                    last = g
                return g
            await pg.cdp("Input.dispatchMouseEvent", type="mouseWheel", x=bg[0], y=bg[1], deltaX=0, deltaY=delta)
            await asyncio.sleep(0.15)
        return None

    async def press(pg, name: str, g: dict | None) -> None:
        if not g:
            r[name] = False
            return
        cw, ch = g["w"] / g["cols"], g["h"] / g["rows"]
        x, y = g["x"] + (COL - 0.5) * cw, g["y"] + (ROW - 0.5) * ch
        before = len(clicks())
        await pg.mouse("mousePressed", x, y)
        await pg.mouse("mouseReleased", x, y)
        for _ in range(30):
            if len(clicks()) > before:
                break
            await asyncio.sleep(0.1)
        got = clicks()[before:]
        info[name] = {"scale": round(g["scale"], 3), "at": [round(x), round(y)], "got": got[:1]}
        r[name] = got[:1] == [f"{COL} {ROW}"]

    async def drive() -> None:
        async with rig.page(1400, 900) as pg:
            r["ready"] = bool(await pg.until(READY, 20))
            await asyncio.sleep(1.0)  # the first fit's resizes answered
            await press(pg, "zoom_1", await pg.js(GRID))
            await press(pg, "zoomed_in", await zoom_until(pg, -100, lambda s: s > 1.5))
            await press(pg, "zoomed_out", await zoom_until(pg, 100, lambda s: s < 0.7))
            info["page_errors"] = [e for e in pg.logs if "EXC" in e][:3]

    try:
        asyncio.run(drive())
    finally:
        rig.stop()
    ok = bool(r) and all(r.values())
    rig.summary["checks"], rig.summary["info"] = r, info
    note = "a press lands on (%d, %d) at scale %s" % (COL, ROW, ", ".join(str(info[k]["scale"]) for k in ("zoom_1", "zoomed_in", "zoomed_out"))) \
        if ok else f"failed: {', '.join(k for k, v in r.items() if not v)} · {info}"
    return verdict(*rig.finish(ok, note))


if __name__ == "__main__":
    sys.exit(0 if main() else 1)
