"""E53 panel_types — html, GL, terminal, stream, set-up: render + survive reloads."""

from __future__ import annotations

import asyncio
import re
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[2]))
from _lib.canvas_live import Rig, verdict, answer, ask, html_cell, http_get, in_frame, say, spot, wait_quiet  # noqa: E402,F401

# every terminal's buffer, as text
TERM_TEXT = ("((()=>[...document.querySelectorAll('.agent-frame .frame-body')].map(f=>{const t=f.xterm; if(!t) return ''; "
             "const b=t.buffer.active; let s=''; for(let i=0;i<b.length;i++){const l=b.getLine(i); if(l) s+=l.translateToString(true)+' ';} return s;}).join(' '))())")


def main() -> bool:
    rig = Rig(__file__, live=False)
    rig.build()
    rig.make_home(None)
    rig.init(cells=[html_cell("h1", "Panel", 700, 60, "<p id='x'>html-53</p>"),
                    {"id": "g1", "handler_module": "gl_agent.ts", "display_name": "Scene", "x": 1100, "y": 60, "width": 280,
                     "height": 200, "gl_source": "scene.add(new THREE.Mesh(new THREE.PlaneGeometry(1,1)));"}])

    async def drive():
        async with rig.page(1800, 1000) as pg:
            await spot(pg, 700, 450, "terminal")
            await pg.until("!!document.querySelector('.agent-frame .xterm')", 20)
            # keys register only once the shell has loaded (owner 2026-10-13): its
            # prompt drawn, not a guessed wait
            await pg.until(f"{TERM_TEXT}.trim().length > 0", 30)
            await pg.js("document.querySelector('.agent-frame .xterm textarea')?.focus()")
            await pg.cdp("Input.insertText", text="echo MARK-53")
            await pg.cdp("Input.dispatchKeyEvent", type="keyDown", key="Enter", code="Enter", windowsVirtualKeyCode=13)
            # the shell echoed it (a reload before that has nothing to replay)
            echoed = await pg.until(f"{TERM_TEXT}.includes('MARK-53')", 20)
            # what the terminal showed before the reloads (a failure's evidence)
            diag = {"echoed": echoed, "term_before": ((await pg.js(TERM_TEXT)) or "").strip()[:600],
                    "overlay_before": await pg.js("[...document.querySelectorAll('.term-stopped')].filter(o=>!o.hidden).map(o=>o.textContent.trim()).join(' | ')")}
            await spot(pg, 1250, 450, "stream")
            await asyncio.sleep(1)
            await spot(pg, 250, 700, "AI agent")  # none set up: the wizard
            await pg.until("!!document.querySelector('.wiz .tiles button')", 20)
            await asyncio.sleep(2)  # the loader's flush lands before any reload
            ids = lambda: pg.js("[...document.querySelectorAll('.agent-frame')].map(f=>f.dataset.id).sort()")
            before = await ids()
            seen = []
            for _ in range(2):
                await pg.open(rig.url)
                await pg.until(f"document.querySelectorAll('.agent-frame').length >= {len(before)}", rig.limits["seconds"])
                await asyncio.sleep(3)
                seen.append({
                    "ids": await ids(),
                    "term": await pg.js(f"{TERM_TEXT}.includes('MARK-53')"),
                    "html": await pg.js("[...document.querySelectorAll('.agent-frame iframe')].some(f=>{try{return f.contentDocument.body.innerText.includes('html-53')}catch(e){return false}})"),
                    "setup": await pg.js("!!document.querySelector('.wiz .tiles button')"),
                    "stream": await pg.js("!!document.querySelector('.stream-view')"),
                    "term_text": ((await pg.js(TERM_TEXT)) or "").strip()[:300],
                    "overlay": await pg.js("[...document.querySelectorAll('.term-stopped')].filter(o=>!o.hidden).map(o=>o.textContent.trim()).join(' | ')"),
                })
            errors = [e for e in pg.logs if "WebGL" not in e and "EXC" in e]
            return before, seen, errors, diag
    try:
        before, seen, errors, diag = asyncio.run(drive())
    finally:
        rig.stop()
    rig.summary.update({"before": before, "after": seen, "errors": errors, "terminal": diag})
    print(f"frames {before} · after reloads {seen} · errors {errors[:3]}")
    ok = all(s["ids"] == before and s["term"] and s["html"] and s["setup"] and s["stream"] for s in seen) and not errors
    return verdict(*rig.finish(ok, f"{len(before)} kinds of cell, same after 2 reloads, terminal replay + html + set-up + stream intact: {ok}"))


if __name__ == "__main__":
    sys.exit(0 if main() else 1)
