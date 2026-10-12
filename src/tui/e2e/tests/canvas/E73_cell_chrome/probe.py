"""E73 cell chrome (owner 2026-10-11): the chrome shows when the pointer is just
OUTSIDE a cell, on its real border (not only over the content); a handle on
every corner resizes it with the opposite corner fixed; and the hover costs
nothing measurable — the frame rate and the GPU process's CPU while the pointer
sweeps over the cells match the pointer at rest, over an animated background."""

from __future__ import annotations

import asyncio
import json
import subprocess
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[2]))
sys.path.insert(0, str(Path(__file__).resolve().parents[6] / "e2e" / "runners"))
from _lib.canvas_live import Rig, html_cell, verdict  # noqa: E402

BG = """
const m = new THREE.ShaderMaterial({uniforms:{u:{value:0}}, depthTest:false, depthWrite:false,
  vertexShader:'void main(){gl_Position=vec4(position.xy,0.,1.);}',
  fragmentShader:'uniform float u; void main(){vec2 p=gl_FragCoord.xy/900.; gl_FragColor=vec4(0.5+0.5*sin(u+p.x*9.),0.3,0.5+0.5*cos(u+p.y*7.),1.);}'});
const q = new THREE.Mesh(new THREE.PlaneGeometry(2,2), m); q.frustumCulled=false; q.renderOrder=-1; scene.add(q);
onFrame(t => { m.uniforms.u.value = t; });
cleanup.push(() => { q.geometry.dispose(); m.dispose(); });
"""
FPS = "new Promise(res=>{let n=0;const t=performance.now();const f=()=>{n++; if(performance.now()-t<1000) requestAnimationFrame(f); else res(n)}; requestAnimationFrame(f)})"
HEAD_SHOWN = "(id)=>getComputedStyle(document.querySelector(`.agent-frame[data-id=\"${id}\"] > .frame-head`)).opacity"


def gpu_cpu() -> float:
    rows = subprocess.run(["ps", "-A", "-o", "pcpu=,args="], capture_output=True, text=True).stdout.splitlines()
    return sum(float(x.split(None, 1)[0]) for x in rows if "--type=gpu-process" in x and "headless" in x)


def main() -> bool:
    rig = Rig(__file__, live=False)
    rig.build()
    rig.make_home(None)
    r: dict = {}
    info: dict = {}
    try:
        rig.init(cells=[html_cell(f"c{i}", f"C{i}", 120 + (i % 3) * 420, 120 + (i // 3) * 360, f"<p style='color:#ccc'>cell {i}</p>", 360, 280) for i in range(6)])

        async def run() -> None:
            from canvas import host_call
            async with rig.page(1400, 900) as pg:
                await pg.until("document.querySelectorAll('.agent-frame').length === 6", 20)
                await host_call(rig.port, f"{rig.page_guid()}|background", {"type": "set_bg", "bytes": BG})
                await asyncio.sleep(3)
                await pg.until("!!document.querySelector('.agent-frame[data-id=c0] .frame-body')", 10)
                body = await pg.rect(".agent-frame[data-id=c0] .frame-body")
                info["c0"] = body
                info["frames_at_measure"] = await pg.js("[...document.querySelectorAll('.agent-frame')].map(f=>f.dataset.id+':'+!!f.querySelector('.frame-body'))")
                # 1. the pointer 10px ABOVE the cell and 10px RIGHT of it: the chrome shows
                await pg.cdp("Input.dispatchMouseEvent", type="mouseMoved", x=700, y=880)
                await asyncio.sleep(0.4)
                info["head_away"] = await pg.js(f"({HEAD_SHOWN})('c0')")
                await pg.cdp("Input.dispatchMouseEvent", type="mouseMoved", x=body["x"] + 60, y=body["y"] - 38)
                await asyncio.sleep(0.8)
                info["head_above"] = await pg.js(f"({HEAD_SHOWN})('c0')")
                await pg.cdp("Input.dispatchMouseEvent", type="mouseMoved", x=700, y=880)
                await asyncio.sleep(0.4)
                await pg.cdp("Input.dispatchMouseEvent", type="mouseMoved", x=body["x"] + body["w"] + 10, y=body["y"] + 60)
                await asyncio.sleep(0.8)
                info["head_right"] = await pg.js(f"({HEAD_SHOWN})('c0')")
                # 2. every corner resizes; the opposite corner stays
                geo = "(id)=>{const f=document.querySelector(`.agent-frame[data-id=\"${id}\"]`); return ['left','top','width','height'].map(k=>parseFloat(f.style[k]))}"
                info["corners"] = {}
                for corner, (dx, dy) in {"nw": (-30, -20), "ne": (30, -20), "sw": (-30, 20), "se": (30, 20)}.items():
                    before = await pg.js(f"({geo})('c4')")
                    f = await pg.rect(".agent-frame[data-id=c4] .frame-body")
                    await pg.cdp("Input.dispatchMouseEvent", type="mouseMoved", x=f["x"] + 40, y=f["y"] + 40)
                    await asyncio.sleep(0.3)
                    h = await pg.rect(f".agent-frame[data-id=c4] > .resize-handle.{corner}")
                    cx, cy = h["x"] + h["w"] / 2, h["y"] + h["h"] / 2
                    await pg.cdp("Input.dispatchMouseEvent", type="mouseMoved", x=cx, y=cy)
                    await pg.mouse("mousePressed", cx, cy)
                    await pg.cdp("Input.dispatchMouseEvent", type="mouseMoved", x=cx + dx, y=cy + dy, buttons=1)
                    await pg.mouse("mouseReleased", cx + dx, cy + dy)
                    await asyncio.sleep(0.3)
                    after = await pg.js(f"({geo})('c4')")
                    info["corners"][corner] = [before, after]
                # 3. the cost of hovering: at rest vs the pointer sweeping over the cells
                async def sample(sweep: bool) -> tuple[int, float]:
                    # frames counted IN the page over ~3 s, the pointer moved (or not)
                    # between counts; the GPU process sampled as it goes
                    await pg.js("(()=>{window.__n=0; window.__t0=performance.now(); const f=()=>{window.__n++; requestAnimationFrame(f)}; if(!window.__counting){window.__counting=1; requestAnimationFrame(f)}})()")
                    cpu, k = [], 0
                    t0 = asyncio.get_event_loop().time()
                    while asyncio.get_event_loop().time() - t0 < 3.0:
                        if sweep:
                            await pg.cdp("Input.dispatchMouseEvent", type="mouseMoved", x=100 + (k * 37) % 1200, y=100 + (k * 53) % 700)
                        k += 1
                        if k % 10 == 0:
                            cpu.append(gpu_cpu())
                        await asyncio.sleep(0.03)
                    fps = await pg.js("Math.round(window.__n * 1000 / (performance.now() - window.__t0))")
                    return fps, round(sum(cpu) / max(len(cpu), 1))
                await pg.cdp("Input.dispatchMouseEvent", type="mouseMoved", x=1390, y=890)
                info["rest"] = await sample(False)
                info["sweep"] = await sample(True)
                info["console"] = [x for x in pg.logs if "WebGL" not in x][:6]

        asyncio.run(run())
        r["chrome_hidden_away"] = info.get("head_away") == "0"
        r["chrome_shows_above_the_cell"] = info.get("head_above") == "1"
        r["chrome_shows_beside_the_cell"] = info.get("head_right") == "1"

        def fixed(corner: str) -> bool:
            b, a = info["corners"][corner]
            l0, t0, w0, h0 = b
            l1, t1, w1, h1 = a
            right0, bottom0, right1, bottom1 = l0 + w0, t0 + h0, l1 + w1, t1 + h1
            grew = w1 != w0 and h1 != h0
            keep = {"nw": (right1 == right0 and bottom1 == bottom0), "ne": (l1 == l0 and bottom1 == bottom0),
                    "sw": (right1 == right0 and t1 == t0), "se": (l1 == l0 and t1 == t0)}[corner]
            return grew and keep
        r["every_corner_resizes"] = all(fixed(c) for c in ("nw", "ne", "sw", "se"))
        (fr, cr), (fs, cs) = info["rest"], info["sweep"]
        r["hover_costs_no_frames"] = fs >= 0.9 * fr
        r["hover_costs_no_gpu"] = cs <= 1.5 * cr + 50
    finally:
        rig.stop()
    rig.summary["checks"] = r
    rig.summary["info"] = info
    print(json.dumps(info, indent=1, default=str)[:2500])
    failed = [k for k, v in r.items() if not v]
    return verdict(*rig.finish(not failed, "all checks pass" if not failed else f"failed: {', '.join(failed)}"))


if __name__ == "__main__":
    sys.exit(0 if main() else 1)
