"""E88 terminal_renderer — the terminals' renderer per webview (phase 7, #52):
see README.

Four streaming terminals (E65's `seeds(4)` and ticker). The Chrome lane (every
OS) checks that auto is WebGL, that the view's `terminal_renderer` switches the
terminals live and persists, that the shell's flag picks DOM, and measures
keystroke → render latency per renderer. The WebKitGTK lane (Linux only, and
required there) opens the same canvas in a real WebKitGTK webview, where auto is
DOM by the user agent, and measures it again."""

from __future__ import annotations

import asyncio
import json
import os
import secrets
import statistics
import sys
import time
from pathlib import Path

import yaml

sys.path.insert(0, str(Path(__file__).resolve().parents[2]))
from _lib.canvas_live import FOLDER, Rig, py_typed, verdict  # noqa: E402
from _lib import webkitgtk  # noqa: E402
from helpers import oslayer  # noqa: E402

N = 4
ROUNDS = 8
GAP_S = 0.15
UNANSWERED_MS = 5000.0
P95_MS = float(os.environ.get("LATENCY_P95_MS", "250"))
VERBS = ["watch", "status", "start", "write", "ack", "resize", "output", "paste_image"]
SHELL = 'window.__FANTASTIC_SHELL__ = Object.freeze({name: "probe", os: "linux", webview: "webkitgtk", backdrop: "web"});'


def seeds(n: int) -> tuple[list[dict], list[tuple[str, dict]]]:
    """E65's `seeds`: n terminal cells, each on its own exec agent."""
    cols = max(1, int(n ** 0.5 + 0.999))
    cells, hosts = [], []
    for i in range(n):
        cid, bid = f"cell_load{i:04d}", f"exec_bridge_load{i:04d}"
        cells.append({"id": cid, "handler_module": "terminal_view.ts", "backend_id": bid, "owns_backend": False,
                      "can_call": {"$self": "*", bid: VERBS}, "callable_by": {"$self": "*"},
                      "x": 20 + (i % cols) * 250, "y": 20 + (i // cols) * 170, "width": 240, "height": 160})
        hosts.append((FOLDER, {"id": bid, "handler_module": "exec_bridge.tools", "cwd": ".", "inherit_env": ["*"], "shell": "login",
                               "ingress_rule": "allow_all_ingress", "cell_id": cid, "can_call": {"$self": "*"},
                               "callable_by": {"$self": "*", "mcp": ["*"], "user": ["*"], "canvas_root": ["*"], "canvas_ws": ["watch"]}}))
    return cells, hosts


CIDS = [c["id"] for c in seeds(N)[0]]
FRAMES = "[...document.querySelectorAll('.agent-frame')].filter(f=>f.querySelector('.frame-body')?.xterm)"
# a terminal's ON-SCREEN text, from xterm's own buffer (either renderer)
TEXT = """((f)=>{const t=f&&f.querySelector('.frame-body')?.xterm; if(!t) return ''; const b=t.buffer.active; let s='';
  for(let i=0;i<t.rows;i++){const l=b.getLine(b.viewportY+i); if(l) s+=l.translateToString(true)+'\\n';} return s;})"""
UP = f"{FRAMES}.length === {N} && {FRAMES}.every(f=>({TEXT})(f).trim().length>0)"
RENDERERS = f"{FRAMES}.map(f=>f.querySelector('.frame-body').dataset.renderer ?? null)"
ON = lambda mode: f"{FRAMES}.length === {N} && {FRAMES}.every(f=>f.querySelector('.frame-body').dataset.renderer === {json.dumps(mode)})"  # noqa: E731
# DOM, completely: no live addon, and xterm's own rows hold the text
DOM_COMPLETE = (f"{ON('dom')} && {FRAMES}.every(f=>{{const b=f.querySelector('.frame-body'); return b.webgl === undefined"
                " && (b.querySelector('.xterm-rows')?.textContent ?? '').trim().length > 0})")
# per frame, what DOM_COMPLETE looks at (a failure's evidence)
DOM_DETAIL = (f"{FRAMES}.map(f=>{{const b=f.querySelector('.frame-body'); return {{renderer: b.dataset.renderer ?? null,"
              " webgl: b.webgl !== undefined, rows: (b.querySelector('.xterm-rows')?.textContent ?? '').trim().length,"
              " canvases: b.querySelectorAll('.xterm-screen canvas').length}})")
WEBGL_RENDERS = "((globalThis.__fantasticWebgl||{}).renders||0)"
# the measurement, installed by one evaluation and polled: per marker, the
# first render that shows its echo (`__ren[mk]`) and that render presented, two
# animation frames later (`__lat[mk]`)
MEASURE = """(()=>{
window.__lat = window.__lat || {};
window.__ren = window.__ren || {};
window.__measure = (cid, mk) => {
  const f = document.querySelector(`.agent-frame[data-id="${cid}"]`), t = f?.querySelector('.frame-body')?.xterm;
  if (!t) return false;
  const shows = () => { const b = t.buffer.active; for (let i = 0; i < t.rows; i++) { const l = b.getLine(b.viewportY + i); if (l && l.translateToString(true).includes(mk)) return true; } return false; };
  const t0 = performance.now();
  const d = t.onRender(() => { if (!shows()) return; d.dispose(); window.__ren[mk] = performance.now() - t0;
    requestAnimationFrame(() => requestAnimationFrame(() => { window.__lat[mk] = performance.now() - t0; })); });
  t.input(': ' + mk + '\\r', true);   // the keystrokes, as typed (xterm's onData), in both browsers alike
  return true;
};
return true;
})()"""
T0 = time.time()


def step(what: str) -> None:
    """Where the probe is, on stderr (a CI log keeps it)."""
    print(f"E88 +{time.time() - T0:.0f}s {what}", file=sys.stderr, flush=True)


FPS = "new Promise(res=>{let n=0;const t=performance.now();const f=()=>{n++; if(performance.now()-t<1000) requestAnimationFrame(f); else res(n)}; requestAnimationFrame(f)})"


def stats(v: list[float]) -> dict:
    v = sorted(v)
    return {"p50": round(statistics.median(v)), "p95": round(v[int(len(v) * 0.95) - 1]), "max": round(v[-1])}


async def measure(page, cids: list[str]) -> dict:
    """ROUNDS × the terminals, GAP_S apart: each sample's keystroke → the first
    render that shows its echo (`render`), and → that render presented, two
    frames later (p50/p95/max). One unanswered after UNANSWERED_MS counts as
    that. And one 1 s frame-rate sample."""
    await page.js(MEASURE)
    sent: list[tuple[str, float]] = []
    for _ in range(ROUNDS):
        for cid in cids:
            mk = "MK" + secrets.token_hex(3)
            if not await page.js(f"window.__measure({json.dumps(cid)}, {json.dumps(mk)})"):
                raise RuntimeError(f"no terminal {cid} to measure")
            sent.append((mk, time.time()))
            await asyncio.sleep(GAP_S)
    got: dict[str, tuple[float, float]] = {}
    while len(got) < len(sent):
        seen = await page.js("[window.__ren, window.__lat]") or [{}, {}]
        ren, lat = seen[0] or {}, seen[1] or {}
        for mk, at in sent:
            if mk in got:
                continue
            if mk in lat:
                got[mk] = (float(ren[mk]), float(lat[mk]))
            elif time.time() - at > UNANSWERED_MS / 1000:
                got[mk] = (UNANSWERED_MS, UNANSWERED_MS)
        await asyncio.sleep(0.05)
    v = [got[mk][1] for mk, _ in sent]
    fps = await page.js(FPS)
    return {**stats(v), "render": stats([got[mk][0] for mk, _ in sent]), "n": len(v),
            "unanswered": sum(1 for x in v if x >= UNANSWERED_MS), "fps": fps}


def main() -> bool:
    rig = Rig(__file__, live=False)
    rig.build()
    rig.make_home(None)
    cells, hosts = seeds(N)
    # E65's stream: a timestamp every 200 ms (~5 lines/s), in any shell — and it
    # READS its input: a Windows console echoes typed keys only to a program
    # reading them (a unix tty echoes them regardless)
    rig.proj.mkdir(parents=True, exist_ok=True)
    (rig.proj / "ticker.py").write_text(
        "import sys, threading, time\n"
        "threading.Thread(target=lambda: [None for _ in sys.stdin], daemon=True).start()\n"
        "while True:\n    print(f'{time.time():.6f}', flush=True)\n    time.sleep(0.2)\n")
    r: dict = {}
    info: dict = {"latency_p95_ms": P95_MS}
    table: dict = {}
    info["table"] = table
    view_yaml = rig.cells_dir.parent / "agent.yaml"
    linux = sys.platform.startswith("linux")

    def on_disk() -> dict:
        try:
            return yaml.safe_load(oslayer.read_text(view_yaml)) or {}
        except FileNotFoundError:
            return {}

    async def disk_until(want: str, timeout: float = 10.0) -> str | None:
        """The view record's terminal_renderer once it is `want` (the loader's
        write landed), or the last one read."""
        deadline = time.time() + timeout
        got = on_disk().get("terminal_renderer")
        while got != want and time.time() < deadline:
            await asyncio.sleep(0.1)
            got = on_disk().get("terminal_renderer")
        return got

    try:
        rig.init(cells=cells, host_agents=hosts)

        async def run() -> None:
            from canvas import host_call

            view = lambda payload: host_call(rig.port, f"{rig.page_guid()}|view_root", payload)  # noqa: E731

            async def stream(i: int) -> bool:
                # through each backend (its live PTY job's `write`), as E65 does
                bid = f"exec_bridge_load{i:04d}"
                st = await host_call(rig.port, bid, {"type": "status"})
                job = next((j.get("job_id") for j in st.get("jobs", []) if j.get("pty") and j.get("status") == "running"), None)
                if job is None:
                    return False
                w = await host_call(rig.port, bid, {"type": "write", "job_id": job, "data": py_typed("ticker.py") + "\r"})
                return "error" not in w

            # ── the Chrome lane ─────────────────────────────────────────────
            async with rig.page(1400, 900) as pg:
                step('chrome: waiting for 4 terminals')
                r["chrome: 4 terminals up"] = await pg.until(UP, 60)
                started = [await stream(i) for i in range(N)]
                info["streams_started"] = f"{sum(started)}/{N}"
                r["4 terminals streaming 5 lines/s"] = all(started)
                await asyncio.sleep(2)
                # 1: auto is webgl
                a = await view({"type": "terminal_renderer"})
                info["1_auto"] = a
                await pg.until(ON("webgl"), 10)
                info["1_renderers"] = await pg.js(RENDERERS)
                r["auto is webgl in Chrome"] = (a.get("terminal_renderer") == "auto" and a.get("in_use") == "webgl"
                                                and str(a.get("why", "")).startswith("auto: not WebKitGTK")
                                                and info["1_renderers"] == ["webgl"] * N)
                # 2: webgl
                step('chrome: measuring webgl')
                chrome: dict = {"webgl": await measure(pg, CIDS)}
                table["chrome"] = chrome
                # 3: the switch to dom, live
                await pg.js("window.__rProbe = 1")
                info["3_set"] = await view({"type": "terminal_renderer", "name": "dom"})
                t0 = time.time()
                complete = await pg.until(DOM_COMPLETE, 5)
                info["3_seconds"] = round(time.time() - t0, 2)
                info["3_detail"] = await pg.js(DOM_DETAIL)
                g0 = await pg.js(WEBGL_RENDERS)
                await asyncio.sleep(1.0)
                g1 = await pg.js(WEBGL_RENDERS)
                info["3_webgl_renders_over_1s"] = g1 - g0
                no_reload = await pg.js("window.__rProbe === 1")
                r["the switch to dom is live and complete"] = complete and g1 == g0 and no_reload is True and info["3_set"].get("in_use") == "dom"
                info["3_disk"] = await disk_until("dom")
                # 4: dom
                step('chrome: measuring dom')
                chrome["dom"] = await measure(pg, CIDS)
                # 5: auto comes back
                info["5_set"] = await view({"type": "terminal_renderer", "name": "auto"})
                r["auto comes back"] = await pg.until(ON("webgl"), 5) and info["5_set"].get("in_use") == "webgl"
                info["5_disk"] = await disk_until("auto")
                r["the setting persists on the view"] = info["3_disk"] == "dom" and info["5_disk"] == "auto"
                chrome["auto"] = chrome["webgl"]
                r["every Chrome-lane sample answered"] = all(chrome[k]["unanswered"] == 0 for k in ("webgl", "dom"))
                # the ABSOLUTE bound is on keystroke → the render that shows the
                # echo: the two frames the presented mark adds are the runner's
                # frame rate (100–200 ms at Windows CI's 10 fps), not the input path
                r[f"Chrome auto keystroke → render p95 ≤ {P95_MS:g} ms"] = chrome["auto"]["render"]["p95"] <= P95_MS
                step('chrome: the flag path')
                # 6: the flag path
                await pg.cdp("Page.addScriptToEvaluateOnNewDocument", source=SHELL)
                await pg.open(rig.url)
                await pg.until("!!document.getElementById('canvas')", 30)
                await pg.until(UP, 60)
                f = await view({"type": "terminal_renderer"})
                info["6_flag"] = f
                drawn = await pg.until(ON("dom"), 10)
                info["6_renderers"] = await pg.js(RENDERERS)
                r["the shell's flag picks dom"] = drawn and f.get("in_use") == "dom" and f.get("why") == "auto: the shell's webview is webkitgtk"
                info["chrome_console"] = [x for x in pg.logs if "WebGL" not in x][:6]

            # ── the WebKitGTK lane (Linux only) ─────────────────────────────
            if not linux:
                info["webkitgtk"] = f"not run: the WebKitGTK lane is Linux only (this is {sys.platform})"
                return
            why = webkitgtk.missing()
            if why is not None:
                r[f"WebKitGTK: {webkitgtk.INSTALL}"] = False
                info["webkitgtk"] = why
                return
            step('webkitgtk: starting')
            wk = webkitgtk.WebKitGtkPage()
            try:
                await wk.start(rig.url, width=1400, height=900)
                info["webkitgtk_ua"] = wk.ua
                await wk.until("!!document.getElementById('canvas')", 30)
                step('webkitgtk: waiting for 4 terminals')
                r["webkitgtk: 4 terminals up"] = await wk.until(UP, 60)
                # 7: auto is dom by the user agent
                a = await view({"type": "terminal_renderer"})
                info["7_auto"] = a
                drawn = await wk.until(ON("dom"), 10)
                info["7_renderers"] = await wk.js(RENDERERS)
                r["auto is dom in WebKitGTK by its user agent"] = drawn and a.get("in_use") == "dom" and a.get("why") == "auto: the user agent is WebKitGTK's"
                # 8: dom (auto)
                step('webkitgtk: measuring dom')
                gtk: dict = {"dom": await measure(wk, CIDS)}
                gtk["auto"] = gtk["dom"]
                table["webkitgtk"] = gtk
                # 9: webgl, information only
                info["9_set"] = await view({"type": "terminal_renderer", "name": "webgl"})
                await wk.until(ON("webgl"), 10)
                info["9_renderers"] = await wk.js(RENDERERS)
                step('webkitgtk: measuring webgl')
                gtk["webgl"] = await measure(wk, CIDS)
                info["9_back"] = await view({"type": "terminal_renderer", "name": "auto"})
                await wk.until(ON("dom"), 10)
                r[f"WebKitGTK auto keystroke → render p95 ≤ {P95_MS:g} ms"] = gtk["auto"]["render"]["p95"] <= P95_MS
                # the RELATIVE bound keeps the presented mark: a slow presentation
                # path is WebKitGTK's risk, measured against Chrome on the same runner
                bound = 1.5 * table["chrome"]["auto"]["p95"] + 20
                info["webkitgtk_vs_chrome_bound_ms"] = bound
                r["WebKitGTK auto presented p95 ≤ 1.5 × Chrome auto presented p95 + 20 ms"] = gtk["auto"]["p95"] <= bound
                info["webkitgtk_misses"] = wk.misses[-6:]
            finally:
                step('webkitgtk: closing')
                await wk.close()
                info["webkitgtk_log"] = [x for x in wk.logs if "libEGL" not in x][-6:]

        asyncio.run(run())
    finally:
        rig.stop()
    print(json.dumps(info, indent=1, default=str)[:6000])
    rig.summary["checks"] = r
    rig.summary["info"] = info
    failed = [k for k, v in r.items() if not v]
    # the latency table in the verdict line: a CI log keeps it, pass or fail
    lat = " · ".join(f"{lane} {k} render p50/p95 {m['render']['p50']}/{m['render']['p95']}, presented p50/p95 {m['p50']}/{m['p95']} ms, {m['fps']} fps"
                     for lane, rows in table.items() for k, m in rows.items() if k != "auto")
    note = f"{len(r) - len(failed)}/{len(r)} pass" + ("" if not failed else f" — failed: {'; '.join(failed)}") + (f" — {lat}" if lat else "")
    return verdict(*rig.finish(bool(r) and not failed, note))


if __name__ == "__main__":
    sys.exit(0 if main() else 1)
