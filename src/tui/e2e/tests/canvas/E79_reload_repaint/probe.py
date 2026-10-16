"""E79 reload_repaint — a page reload re-attaches a terminal whose program is
full-screen (owner 2026-10-15: after a reload `claude` came back, `htop` and
`sudo mactop` stayed blank):

a) a terminal cell runs PAINTER: alternate screen, and on SIGWINCH (polled on
   Windows, which has none) it reads the size and repaints `PAINT-<n>` ONLY if the
   size differs from its last paint — the way curses programs (htop) do;
b) the page reloads; the re-attach shrinks the PTY by a row and restores it;
c) PAINTER painted again (its counter rose) and its paint is on screen — two
   resizes landing before the program reads the size would leave it unchanged
   and nothing repainted."""

from __future__ import annotations

import asyncio
import re
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[2]))
from _lib.canvas_live import FOLDER, Rig, verdict, py_job  # noqa: E402

PAINTER = r'''import os, signal, sys, time
n, last, flag = 0, None, [True]
# A curses program hears SIGWINCH; a console without it (Windows) reads its size
# every tick — either way it repaints only on a CHANGED size.
winch = getattr(signal, "SIGWINCH", None)
if winch is not None:
    signal.signal(winch, lambda *a: flag.__setitem__(0, True))
sys.stdout.write("\x1b[?1049h")
while True:
    if flag[0] or winch is None:
        flag[0] = False
        size = os.get_terminal_size()
        if size != last:
            last, n = size, n + 1
            sys.stdout.write(f"\x1b[H\x1b[2JPAINT-{n} {size.lines}x{size.columns}")
            sys.stdout.flush()
            with open("paints.log", "a") as log:
                log.write(f"{time.time():.3f} {n} {size.lines}x{size.columns}\n")
    time.sleep(0.05)
'''
CELL, BACKEND = "cell_paint", "exec_bridge_paint"
VERBS = ["watch", "status", "start", "write", "ack", "resize", "output"]
# the terminal's ON-SCREEN text, from xterm's own buffer
TERM_TEXT = ("(()=>{const f=document.querySelector('.agent-frame[data-id=\"%s\"] .frame-body'); const t=f&&f.xterm; if(!t) return '';"
             " const b=t.buffer.active; let s=''; for(let i=0;i<b.length;i++){const l=b.getLine(i); if(l) s+=l.translateToString(true)+' ';} return s;})()" % CELL)


def paints(text: str) -> int:
    return max((int(m) for m in re.findall(r"PAINT-(\d+)", text or "")), default=0)


def main() -> bool:
    rig = Rig(__file__, live=False)
    rig.build()
    rig.make_home(None)
    rig.proj.mkdir(parents=True, exist_ok=True)
    (rig.proj / "painter.py").write_text(PAINTER)
    cell = {"id": CELL, "handler_module": "terminal_view.ts", "backend_id": BACKEND, "owns_backend": False,
            "command": py_job("painter.py")[0], "can_call": {"$self": "*", BACKEND: VERBS}, "callable_by": {"$self": "*"},
            "x": 40, "y": 40, "width": 520, "height": 320}
    host = {"id": BACKEND, "handler_module": "exec_bridge.tools", "cwd": ".", "inherit_env": ["*"], "shell": "login",
            "ingress_rule": "allow_all_ingress", "allowed": [py_job("painter.py")[1]], "cell_id": CELL, "can_call": {"$self": "*"},
            "callable_by": {"$self": "*", "mcp": ["*"], "user": ["*"], "canvas_root": ["*"], "canvas_ws": ["watch"]}}
    rig.init(cells=[cell], host_agents=[(FOLDER, host)])
    r: dict = {}
    info: dict = {}

    async def drive() -> None:
        async with rig.page(1400, 900) as pg:
            r["painted"] = bool(await pg.until(f"/PAINT-\\d+/.test({TERM_TEXT})", 20))
            await asyncio.sleep(1.5)  # every resize of the first fit answered
            before = paints(await pg.js(TERM_TEXT))
            info["before"] = before
            info["reload_at"] = f"{__import__('time').time():.3f}"
            await pg.open(rig.url)
            r["reattached"] = bool(await pg.until(f"/PAINT-\\d+/.test({TERM_TEXT})", 20))
            await asyncio.sleep(2.0)
            text = await pg.js(TERM_TEXT) or ""
            info["after"], info["screen"] = paints(text), text.strip()[:120]
            r["repainted"] = info["after"] > before
            info["page_errors"] = [e for e in pg.logs if "EXC" in e][:3]

    try:
        asyncio.run(drive())
    finally:
        rig.stop()
    log = rig.proj / "paints.log"
    info["paints"] = log.read_text().splitlines() if log.is_file() else []
    ok = bool(r) and all(r.values())
    rig.summary["checks"], rig.summary["info"] = r, info
    note = f"paints {info.get('before')} → {info.get('after')} across the reload" if ok else \
        f"failed: {', '.join(k for k, v in r.items() if not v)} · {info}"
    return verdict(*rig.finish(ok, note))


if __name__ == "__main__":
    sys.exit(0 if main() else 1)
