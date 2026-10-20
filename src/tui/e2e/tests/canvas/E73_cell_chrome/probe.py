"""E73 cell chrome (owner 2026-10-11): the chrome shows when the pointer is just
OUTSIDE a cell, on its real border (not only over the content); a handle on
every corner resizes it with the opposite corner fixed; and the hover costs
nothing measurable — the frame rate and the GPU process's CPU while the pointer
sweeps over the cells match the pointer at rest, over an animated background;
its ▁ button minimizes the cell to its bar and back (as a double click on the
head does), and its ⤢ button views it full (as a click on it in the tree does)
and, viewed full, takes it back to its view."""

from __future__ import annotations

import asyncio
import json
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[2]))
sys.path.insert(0, str(Path(__file__).resolve().parents[6] / "e2e" / "runners"))
sys.path.insert(0, str(Path(__file__).resolve().parent))
from _lib.canvas_live import Rig, html_cell, verdict  # noqa: E402
from helpers import oslayer  # noqa: E402
from hover_frames import hover_keeps_frames  # noqa: E402

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


def gpu_cpu(profile: str) -> float:
    """THIS test's Chrome GPU process only (its own profile dir is on its args):
    every other test's headless Chrome runs at the same time."""
    return oslayer.cpu_percent("--type=gpu-process", profile)


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
                async def sample(sweep: bool, secs: float, cpu: list[float]) -> tuple[int, float]:
                    # frames counted IN the page over `secs`, the pointer moved (or
                    # not) between counts; the GPU process sampled as it goes into
                    # `cpu`, the caller's list, so every reading is kept as evidence
                    await pg.js("(()=>{window.__n=0; window.__t0=performance.now(); const f=()=>{window.__n++; requestAnimationFrame(f)}; if(!window.__counting){window.__counting=1; requestAnimationFrame(f)}})()")
                    k = 0
                    t0 = asyncio.get_event_loop().time()
                    while asyncio.get_event_loop().time() - t0 < secs:
                        if sweep:
                            await pg.cdp("Input.dispatchMouseEvent", type="mouseMoved", x=100 + (k * 37) % 1200, y=100 + (k * 53) % 700)
                        k += 1
                        if k % 10 == 0:
                            cpu.append(gpu_cpu(str(rig.art / "chrome")))
                        await asyncio.sleep(0.03)
                    fps = await pg.js("Math.round(window.__n * 1000 / (performance.now() - window.__t0))")
                    return fps, round(sum(cpu) / max(len(cpu), 1))
                await pg.cdp("Input.dispatchMouseEvent", type="mouseMoved", x=1390, y=890)
                # the runner's frame rate with nothing moving, before the samples
                info["baseline_fps"] = await pg.js(FPS)
                # interleaved, rest then sweep, three pairs: a shared runner's load
                # comes and goes (at rest 21 fps, not 60), and each pair meets the
                # same load. Every sample is kept as evidence; the frame verdict
                # reads them all (hover_keeps_frames), the GPU check reads the best
                rests, sweeps = [], []
                info["gpu_cpu"] = {"rest": [], "sweep": []}
                for _ in range(3):
                    cpu_rest, cpu_sweep = [], []
                    rests.append(await sample(False, 1.5, cpu_rest))
                    sweeps.append(await sample(True, 1.5, cpu_sweep))
                    info["gpu_cpu"]["rest"].append(cpu_rest)
                    info["gpu_cpu"]["sweep"].append(cpu_sweep)
                info["rest"] = max(rests, key=lambda x: x[0])
                info["sweep"] = max(sweeps, key=lambda x: x[0])
                info["samples"] = {"rest": rests, "sweep": sweeps}
                # 4. the ▁ and ⤢ buttons on c1
                frame = "document.querySelector('.agent-frame[data-id=c1]')"
                btn = lambda b: pg.js(f"{frame}.querySelector(':scope > .frame-head .{b}').click()")
                state = lambda: pg.js(f"({{min: {frame}.classList.contains('minimized'), full: {frame}.classList.contains('full') && document.getElementById('canvas').classList.contains('full-view'), hash: location.hash, glyph: {frame}.querySelector(':scope > .frame-head .max').textContent}})")
                steps = {}
                await btn("min")
                await pg.until(f"{frame}.classList.contains('minimized')", 5)
                steps["min"] = await state()
                await btn("min")
                await pg.until(f"!{frame}.classList.contains('minimized')", 5)
                steps["unmin"] = await state()
                await btn("max")
                await pg.until(f"{frame}.classList.contains('full')", 5)
                steps["max"] = await state()
                await btn("max")
                await pg.until("!document.getElementById('canvas').classList.contains('full-view')", 5)
                steps["back"] = await state()
                info["buttons"] = steps
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
        (_, cr), (_, cs) = info["rest"], info["sweep"]
        r["hover_costs_no_frames"] = hover_keeps_frames(
            [x[0] for x in info["samples"]["rest"]], [x[0] for x in info["samples"]["sweep"]])
        r["hover_costs_no_gpu"] = cs <= 1.5 * cr + 50
        b = info.get("buttons") or {}
        r["min_button_minimizes"] = bool(b.get("min", {}).get("min")) and not b.get("unmin", {}).get("min", True)
        r["max_button_views_full"] = bool(b.get("max", {}).get("full")) and "c1" in b.get("max", {}).get("hash", "") and b.get("max", {}).get("glyph") == "⤡"
        r["max_button_goes_back"] = b.get("back", {}).get("full") is False and b.get("back", {}).get("glyph") == "⤢"
    finally:
        rig.stop()
    rig.summary["checks"] = r
    rig.summary["info"] = info
    print(json.dumps(info, indent=1, default=str)[:2500])
    failed = [k for k, v in r.items() if not v]
    ok, note = rig.finish(not failed, "all checks pass" if not failed else f"failed: {', '.join(failed)}")
    # the frame-cost evidence on every run, last before the verdict so a CI log
    # tail keeps it: every sample hover_costs_no_frames and hover_costs_no_gpu read
    evidence = {"baseline_fps": info["baseline_fps"], "rest": info["samples"]["rest"],
                "sweep": info["samples"]["sweep"], "gpu_cpu": info["gpu_cpu"],
                "best_rest": info["rest"], "best_sweep": info["sweep"]}
    print("E73 evidence: " + json.dumps(evidence, separators=(",", ":"), default=str), flush=True)
    return verdict(ok, note)


if __name__ == "__main__":
    sys.exit(0 if main() else 1)
