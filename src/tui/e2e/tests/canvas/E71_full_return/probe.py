"""E71 full return — an agent viewed full, then Esc back to its canvas (owner
2026-10-11: "agent → canvas: for a moment the agents show over the old
background"). The order must be: 1) the full agent goes, 2) the background
draws, 3) the cells show. Measured in the page, every animation frame: the
WebGL draw calls on the background canvas, and whether the world (the cells) is
visible — no frame may show a cell before the background's first draw."""

from __future__ import annotations

import asyncio
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[2]))
sys.path.insert(0, str(Path(__file__).resolve().parents[6] / "e2e" / "runners"))
from _lib.canvas_live import Rig, html_cell, verdict  # noqa: E402

BG = """
const m = new THREE.ShaderMaterial({uniforms:{u:{value:0}}, depthTest:false, depthWrite:false,
  vertexShader:'void main(){gl_Position=vec4(position.xy,0.,1.);}',
  fragmentShader:'uniform float u; void main(){gl_FragColor=vec4(0.6,0.1,0.5+0.3*sin(u),1.);}'});
const q = new THREE.Mesh(new THREE.PlaneGeometry(2,2), m); q.frustumCulled=false; scene.add(q);
onFrame(t => { m.uniforms.u.value = t; });
cleanup(() => { q.geometry.dispose(); m.dispose(); });
"""
# every draw call on the background's canvas, counted
COUNT_DRAWS = """(()=>{window.__draws=0; for (const C of [window.WebGLRenderingContext, window.WebGL2RenderingContext]) { if (!C) continue;
  for (const f of ['drawArrays','drawElements']) { const o=C.prototype[f]; C.prototype[f]=function(...a){ if (this.canvas && this.canvas.id==='bg') window.__draws++; return o.apply(this,a); }; } }})()"""
# from now: one sample per animation frame — the draws so far, and the cells on screen
WATCH = """(()=>{window.__s=[]; const w=document.querySelector('.canvas-world'); const tick=()=>{
  const shown=[...document.querySelectorAll('.agent-frame')].filter(f=>getComputedStyle(f).visibility==='visible').length;
  window.__s.push([window.__draws, getComputedStyle(w).visibility==='visible' ? shown : 0, document.getElementById('canvas').classList.contains('full-view')]);
  if (window.__s.length<120) requestAnimationFrame(tick); }; requestAnimationFrame(tick); return window.__draws;})()"""


import os
THROTTLE = float(os.environ.get("E71_THROTTLE", "8"))


def main() -> bool:
    rig = Rig(__file__, live=False)
    rig.build()
    rig.make_home(None)
    r: dict = {}
    info: dict = {}
    try:
        rig.init(cells=[html_cell("a", "A", 100, 100, "<div style='background:#fff;height:100%'>A</div>", 300, 200),
                        html_cell("b", "B", 500, 300, "<div style='background:#fff;height:100%'>B</div>", 300, 200)])

        async def run() -> None:
            from canvas import host_call
            async with rig.page(1000, 700) as pg:
                await pg.cdp("Page.addScriptToEvaluateOnNewDocument", source=COUNT_DRAWS)
                await pg.until("document.querySelectorAll('.agent-frame').length === 2", 20)
                bg = await host_call(rig.port, f"{rig.page_guid()}|background", {"type": "set_bg", "bytes": BG})
                info["bg_set"] = bool(bg.get("ok"))
                await pg.js("location.reload()")
                await asyncio.sleep(1.0)
                await pg.until("document.querySelectorAll('.agent-frame').length === 2 && window.__draws > 0", 20)
                # a slow machine: the CPU throttled (the order must hold however long each step takes)
                await pg.cdp("Emulation.setCPUThrottlingRate", rate=THROTTLE)
                for round_ in range(3):
                    await pg.js("location.hash='#view=a'")
                    await pg.until("document.getElementById('canvas').classList.contains('full-view')", 10)
                    await asyncio.sleep(1.0)
                    full_draws = await pg.js("(()=>{const d=window.__draws; return new Promise(res=>setTimeout(()=>res(window.__draws-d), 300))})()")
                    d0 = await pg.js(WATCH)
                    await pg.cdp("Input.dispatchKeyEvent", type="keyDown", key="Escape", code="Escape", windowsVirtualKeyCode=27)
                    await pg.cdp("Input.dispatchKeyEvent", type="keyUp", key="Escape", code="Escape", windowsVirtualKeyCode=27)
                    await pg.until("window.__s.length >= 120", 40)
                    s = await pg.js("window.__s")
                    early = [i for i, (d, shown, _f) in enumerate(s) if shown > 0 and d == d0]
                    first_cells = next((i for i, x in enumerate(s) if x[1] > 0), None)
                    first_draw = next((i for i, x in enumerate(s) if x[0] > d0), None)
                    info[f"round{round_}"] = {"full_view_bg_draws_per_300ms": full_draws, "first_bg_draw_frame": first_draw,
                                              "first_cells_frame": first_cells, "cells_before_bg_frames": len(early),
                                              "cells_at_end": s[-1][1], "full_at_end": s[-1][2]}
                    await asyncio.sleep(0.5)

        asyncio.run(run())
        rounds = [info.get(f"round{i}", {}) for i in range(3)]
        r["bg_set"] = info.get("bg_set", False)
        r["cells_back"] = all(x.get("cells_at_end") == 2 and x.get("full_at_end") is False for x in rounds)
        r["no_cell_before_bg"] = all(x.get("cells_before_bg_frames", 1) == 0 and x.get("first_bg_draw_frame") is not None for x in rounds)
    finally:
        rig.stop()
    rig.summary["checks"] = r
    rig.summary["info"] = info
    failed = [k for k, v in r.items() if not v]
    return verdict(*rig.finish(not failed, "all checks pass" if not failed else f"failed: {', '.join(failed)}"))


if __name__ == "__main__":
    sys.exit(0 if main() else 1)
