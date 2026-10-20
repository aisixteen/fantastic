"""E70 mobile scroll — a phone-wide tab with a live terminal: lines typed into
the shell, then the column scrolled by the wheel in the gutter, burst after
burst. The frame rate and the host's call lines per second must hold — no
slowdown as the scrolling goes on (owner 2026-10-11: "typed 5 lines, scrolled —
it began to slow down")."""

from __future__ import annotations

import asyncio
import os
import sys
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[2]))
from _lib.canvas_live import FOLDER, Rig, html_cell, verdict  # noqa: E402
from helpers import oslayer  # noqa: E402

# an animated background (a full-screen fragment shader, every frame)
BG = """
const m = new THREE.ShaderMaterial({uniforms:{u:{value:0}}, depthTest:false, depthWrite:false,
  vertexShader:'void main(){gl_Position=vec4(position.xy,0.,1.);}',
  fragmentShader:'uniform float u; void main(){vec2 p=gl_FragCoord.xy/900.; gl_FragColor=vec4(0.5+0.5*sin(u+p.x*9.),0.3,0.5+0.5*cos(u+p.y*7.),1.);}'});
const q = new THREE.Mesh(new THREE.PlaneGeometry(2,2), m); q.frustumCulled=false; q.renderOrder=-1; scene.add(q);
onFrame(t => { m.uniforms.u.value = t; });
cleanup.push(() => { q.geometry.dispose(); m.dispose(); });
"""
WIDTH = int(os.environ.get("E70_WIDTH", "390"))  # 390: a phone (mobile); 1500: desktop (canvas)
VERBS = ["watch", "status", "start", "write", "ack", "resize", "output", "paste_image"]
# frames per second over a 3 s window, divided by the measured elapsed time: a 1 s count at ~20 fps is quantised and noisy
FPS = "new Promise(res=>{let n=0;const t=performance.now();const f=()=>{n++; const e=performance.now()-t; if(e<3000) requestAnimationFrame(f); else res(n*1000/e)}; requestAnimationFrame(f)})"


def seeds() -> tuple[list[dict], list[tuple[str, dict]]]:
    cid, bid = "term", "exec_bridge_term"
    cells = [
        {"id": cid, "handler_module": "terminal_view.ts", "backend_id": bid, "owns_backend": False,
         "can_call": {"$self": "*", bid: VERBS}, "callable_by": {"$self": "*"}, "x": 100, "y": 100, "width": 480, "height": 320},
        html_cell("a", "A", 700, 100, "<p>A</p>", 340, 240),
        html_cell("b", "B", 100, 500, "<p>B</p>", 300, 200),
    ]
    hosts = [(FOLDER, {"id": bid, "handler_module": "exec_bridge.tools", "cwd": ".", "inherit_env": ["*"], "shell": "login",
                       "ingress_rule": "allow_all_ingress", "cell_id": cid, "can_call": {"$self": "*"},
                       "callable_by": {"$self": "*", "mcp": ["*"], "user": ["*"], "canvas_root": ["*"], "canvas_ws": ["watch"]}})]
    return cells, hosts


async def type_line(pg, text: str) -> None:
    await pg.js("document.querySelector('.agent-frame[data-id=\"term\"] .xterm-helper-textarea')?.focus()")
    await pg.cdp("Input.insertText", text=text)
    await pg.cdp("Input.dispatchKeyEvent", type="keyDown", key="Enter", code="Enter", windowsVirtualKeyCode=13, text="\r")
    await pg.cdp("Input.dispatchKeyEvent", type="keyUp", key="Enter", code="Enter", windowsVirtualKeyCode=13)


def main() -> bool:
    rig = Rig(__file__, live=False)
    rig.build()
    rig.make_home(None)
    r: dict = {}
    info: dict = {}
    try:
        cells, hosts = seeds()
        rig.init(cells=cells, host_agents=hosts)

        async def run() -> None:
            async with rig.page(WIDTH, 800) as pg:
                await pg.until("document.querySelectorAll('.agent-frame').length === 3", 30)
                await asyncio.sleep(3.0)
                for i in range(5):
                    await type_line(pg, f"echo line {i}")
                    await asyncio.sleep(0.3)
                await asyncio.sleep(1.0)
                # the GPU process's CPU and the frame rate as shipped vs with every backdrop blur removed,
                # over an animated background
                sys.path.insert(0, str(Path(__file__).resolve().parents[6] / "e2e" / "runners"))
                from canvas import host_call
                await host_call(rig.port, f"{rig.page_guid()}|background", {"type": "set_bg", "bytes": BG})
                await asyncio.sleep(3.0)

                def gpu_cpu() -> float:
                    return oslayer.cpu_percent("--type=gpu-process", "headless")

                async def sample(tag: str) -> None:
                    await asyncio.sleep(2.0)
                    cpu = []
                    for _ in range(4):
                        cpu.append(gpu_cpu())
                        await asyncio.sleep(0.5)
                    info[f"{tag}_fps"] = await pg.js(FPS)
                    info[f"{tag}_gpu_cpu"] = round(sum(cpu) / len(cpu))
                if WIDTH >= 768:  # desktop: the chrome (and its blur) shows on the cell pointed at
                    body = await pg.rect(".agent-frame[data-id=term] .frame-body")
                    await pg.cdp("Input.dispatchMouseEvent", type="mouseMoved", x=body["x"] + 20, y=body["y"] + 20)
                await sample("shipped")
                # the chrome carries no backdrop blur (styles.ts:35-37, 67): assert it from computed style, not a timing control
                info["chrome_backdrop"] = await pg.js("(()=>{const v=[]; document.querySelectorAll('.agent-frame').forEach(f=>{v.push(getComputedStyle(f,'::before').backdropFilter); const h=f.querySelector(':scope > .frame-head'); if(h) v.push(getComputedStyle(h).backdropFilter)}); return v;})()")
                cw = (await pg.js("document.getElementById('canvas').clientWidth")) or 500
                # two 3 s windows, averaged: one window is too noisy to gate on
                base = [await pg.js(FPS), await pg.js(FPS)]
                fps, calls = [sum(base) / 2], []
                for burst in range(8):
                    l0, t0 = len(rig.log.read_text().splitlines()), time.time()
                    for k in range(30):
                        dy = 120 if (burst + k // 15) % 2 == 0 else -120
                        await pg.cdp("Input.dispatchMouseEvent", type="mouseWheel", x=4, y=400, deltaX=0, deltaY=dy)
                    fps.append(await pg.js(FPS))
                    calls.append(round((len(rig.log.read_text().splitlines()) - l0) / (time.time() - t0)))
                info["fps"], info["host_lines_per_s"] = fps, calls
                await asyncio.sleep(2.0)
                l0 = len(rig.log.read_text().splitlines())
                await asyncio.sleep(3.0)
                info["idle_host_lines_per_s"] = round((len(rig.log.read_text().splitlines()) - l0) / 3)
                info["fps_idle_after"] = await pg.js(FPS)
                info["console"] = pg.logs[:10]

        asyncio.run(run())
        f = info.get("fps") or [0]
        r["fps_holds"] = min(f[1:] or [0]) >= 0.7 * f[0] and info.get("fps_idle_after", 0) >= 0.7 * f[0]
        # the phone's always-shown chrome costs no backdrop blur (it did: 658% vs 185%)
        cb = info.get("chrome_backdrop") or []
        r["no_blur_cost"] = len(cb) >= 3 and all(v == "none" for v in cb)  # only an explicit none: an unread style must not pass
        r["host_quiet_at_rest"] = info.get("idle_host_lines_per_s", 999) < 5
    finally:
        rig.stop()
    rig.summary["checks"] = r
    rig.summary["info"] = info
    failed = [k for k, v in r.items() if not v]
    return verdict(*rig.finish(not failed, "all checks pass" if not failed else f"failed: {', '.join(failed)}"))


if __name__ == "__main__":
    sys.exit(0 if main() else 1)
