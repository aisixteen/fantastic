"""E65 load — a canvas under load, MEASURED (owner 2026-10-09: input and visual
lag minimal, the PTY fit right, disk use minimal).

N terminal cells (LOAD_N, default 10; step 10 → 100 → 1000), each a login shell
on its own exec_bridge, every one streaming a line every 200 ms (output) while
keystrokes are typed into a sample of them (input), over an animated shader
background. Over LOAD_SECONDS (default 20) of load it measures:

- input → echo: a marker typed into a sample terminal until it is on that
  terminal's screen (keystroke → PTY → output → xterm), p50 / p95 / max;
- the page's frame rate (requestAnimationFrame) under the load;
- the disk: the AI store's growth (`memory.db` + its WAL) per second, and that
  NO kernel execution row reaches it (the kernel's store is RAM, `kernel_memory`);
- the host's call stream: lines per second (one per call).

THE DISK GUARD: the load stops the moment the store passes LOAD_DISK_MB
(default 200) — the probe never fills the disk. Each terminal's PTY fit is
checked too (the grid inside the visible area)."""

from __future__ import annotations

import asyncio
import json
import os
import secrets
import sqlite3
import statistics
import sys
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[2]))
from _lib.canvas_live import FOLDER, Rig, py_typed, verdict  # noqa: E402

N = int(os.environ.get("LOAD_N", "10"))
SECONDS = float(os.environ.get("LOAD_SECONDS", "20"))
DISK_MB = float(os.environ.get("LOAD_DISK_MB", "200"))
SAMPLES = min(N, 5)
WITH_BG = os.environ.get("LOAD_BG", "1") != "0"
VERBS = ["watch", "status", "start", "write", "ack", "resize", "output", "paste_image"]
# a heavy background: a full-screen raymarch-ish fragment shader plus moving meshes
BG = """
const m = new THREE.ShaderMaterial({uniforms:{u:{value:0}}, depthTest:false, depthWrite:false,
  vertexShader:'void main(){gl_Position=vec4(position.xy,0.,1.);}',
  fragmentShader:'uniform float u; void main(){vec2 p=gl_FragCoord.xy/900.; float c=0.;'+
    'for(int i=0;i<48;i++){float f=float(i); vec2 q=p+vec2(sin(u*0.3+f),cos(u*0.2+f*1.3))*0.4; c+=0.004/abs(length(q-0.5)-0.2-0.1*sin(f+u));}'+
    'gl_FragColor=vec4(c*0.2,c*0.4,c,1.);}'});
const q = new THREE.Mesh(new THREE.PlaneGeometry(2,2), m); q.frustumCulled=false; q.renderOrder=-1; scene.add(q);
const g = new THREE.Group(); scene.add(g);
for (let i=0;i<200;i++){ const b=new THREE.Mesh(new THREE.IcosahedronGeometry(1.2,1), new THREE.MeshBasicMaterial({color:0x3355ff, wireframe:true}));
  b.position.set((Math.random()-.5)*300,(Math.random()-.5)*160,(Math.random()-.5)*80); g.add(b); }
onFrame(t => { m.uniforms.u.value = t; g.rotation.y = t*0.1; g.children.forEach((b,i)=>{ b.rotation.x = t+i; }); });
cleanup(() => { q.geometry.dispose(); m.dispose(); });
"""


def seeds(n: int) -> tuple[list[dict], list[tuple[str, dict]]]:
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


def store_bytes(proj: Path) -> int:
    f = proj / ".fantastic"
    return sum((f / n).stat().st_size for n in ("memory.db", "memory.db-wal", "memory.db-shm") if (f / n).exists())


def row_stats(proj: Path, after_ts: float, before_ts: float) -> dict:
    db = sqlite3.connect(f"file:{proj / '.fantastic' / 'memory.db'}?mode=ro", uri=True)
    q = lambda sql, *a: db.execute(sql, a).fetchall()
    w = "json_extract(doc,'$.ts') BETWEEN ? AND ?"
    out = {
        "rows": q(f"select count(*) from rows where {w}", after_ts, before_ts)[0][0],
        "by_kind": dict(q(f"select json_extract(doc,'$.kind'), count(*) from rows where {w} group by 1 order by 2 desc", after_ts, before_ts)),
        "top_calls": [f"{a} {v}: {c}" for a, v, c in q(
            f"select json_extract(doc,'$.agent') a, json_extract(doc,'$.verb') v, count(*) c from rows where {w} and json_extract(doc,'$.kind')='call' "
            "group by 1,2 order by 3 desc limit 12", after_ts, before_ts)],
        "top_settled": [f"{a}: {c}" for a, c in q(
            f"select json_extract(doc,'$.settled'), count(*) from rows where {w} and json_extract(doc,'$.kind')='settle' group by 1 order by 2 desc limit 6",
            after_ts, before_ts)],
    }
    db.close()
    return out


# a terminal's ON-SCREEN text, from xterm's own buffer (the WebGL renderer
# draws to a canvas: there is no text in the DOM)
TEXT = """((f)=>{const t=f&&f.querySelector('.frame-body')?.xterm; if(!t) return ''; const b=t.buffer.active; let s='';
  for(let i=0;i<t.rows;i++){const l=b.getLine(b.viewportY+i); if(l) s+=l.translateToString(true)+'\\n';} return s;})"""
# timed IN THE PAGE: the first RENDER (xterm's onRender) after which the marker is on screen
ARM = """(()=>{const cid=%s, mk=%s; window.__lat=window.__lat||{};
  const f=document.querySelector(`.agent-frame[data-id="${cid}"]`); const t=f&&f.querySelector('.frame-body')?.xterm; if(!t) return;
  const t0=performance.now(); const d=t.onRender(()=>{ if((""" + TEXT + """)(f).includes(mk)){ window.__lat[mk]=performance.now()-t0; d.dispose(); } });})()"""


def system_sample(host_pid: int) -> dict | None:
    """One sample of the machine: load average, and %CPU by who — the host
    (fantastic), Chrome (by its process type), the shells, everything else.
    Read with macOS's own tools (sysctl, ps, ioreg): elsewhere `None`, not
    measured — the load's own numbers (echo, fps, store, rows) still are."""
    if sys.platform != "darwin":
        return None
    import subprocess
    la = subprocess.run(["sysctl", "-n", "vm.loadavg"], capture_output=True, text=True).stdout.strip("{} \n").split()
    rows = subprocess.run(["ps", "-A", "-o", "pid=,ppid=,pcpu=,rss=,comm=,args="], capture_output=True, text=True).stdout.splitlines()
    by: dict[str, list[float]] = {}
    for line in rows:
        parts = line.split(None, 5)
        if len(parts) < 5:
            continue
        pid, ppid, cpu, rss, comm = int(parts[0]), int(parts[1]), float(parts[2]), int(parts[3]), parts[4]
        args = parts[5] if len(parts) > 5 else ""
        if pid == host_pid:
            k = "host (fantastic)"
        elif "Google Chrome" in comm or "Google Chrome" in args:
            k = "chrome gpu" if "--type=gpu-process" in args else "chrome renderer" if "--type=renderer" in args else "chrome other"
        elif comm.endswith("zsh") or comm.endswith("bash") or comm.endswith("/sh"):
            k = "shells"
        else:
            k = "other"
        a = by.setdefault(k, [0.0, 0.0, 0])
        a[0] += cpu
        a[1] += rss / 1024
        a[2] += 1
    import re as _re
    io = subprocess.run(["ioreg", "-r", "-d", "1", "-c", "IOAccelerator"], capture_output=True, text=True).stdout
    gpu = {k: int(v) for k, v in _re.findall(r'"(Device|Renderer|Tiler) Utilization %"=(\d+)', io)}
    total = sum(v[0] for v in by.values())
    return {"load": [float(x) for x in la[:3]], "cpu_total_pct": round(total), "gpu_pct": gpu.get("Device"), "by": {k: {"cpu": round(v[0]), "mb": round(v[1]), "n": v[2]} for k, v in sorted(by.items(), key=lambda x: -x[1][0])}}


async def type_into(pg, cid: str, text: str) -> None:
    await pg.js(f"document.querySelector('.agent-frame[data-id=\"{cid}\"] .xterm-helper-textarea')?.focus()")
    await pg.cdp("Input.insertText", text=text)
    await pg.cdp("Input.dispatchKeyEvent", type="keyDown", key="Enter", code="Enter", windowsVirtualKeyCode=13, text="\r")
    await pg.cdp("Input.dispatchKeyEvent", type="keyUp", key="Enter", code="Enter", windowsVirtualKeyCode=13)


async def profile(pg, secs: float) -> list[str]:
    """The page's main thread for `secs`: the top functions by SELF time (CDP
    Profiler), and the share idle / GC / layout+paint (program) take."""
    await pg.cdp("Profiler.enable")
    await pg.cdp("Profiler.setSamplingInterval", interval=500)
    await pg.cdp("Profiler.start")
    await asyncio.sleep(secs)
    prof = (await pg.cdp("Profiler.stop"))["profile"]
    nodes = {n["id"]: n for n in prof["nodes"]}
    dt = dict.fromkeys(nodes, 0.0)
    for nid, d in zip(prof["samples"], prof.get("timeDeltas", [])):
        dt[nid] += d / 1000.0
    by: dict[str, float] = {}
    for nid, ms in dt.items():
        cf = nodes[nid]["callFrame"]
        key = f"{cf['functionName'] or '(anon)'} {cf['url'].rsplit('/', 1)[-1]}:{cf['lineNumber']}"
        by[key] = by.get(key, 0.0) + ms
    total = sum(by.values()) or 1.0
    return [f"{ms / total * 100:5.1f}% {k}" for k, ms in sorted(by.items(), key=lambda x: -x[1])[:15]]


fps_js0 = "new Promise(res=>{let n=0;const t=performance.now();const f=()=>{n++; if(performance.now()-t<1000) requestAnimationFrame(f); else res(n)}; requestAnimationFrame(f)})"


async def trace(pg, secs: float) -> dict:
    """A Chrome trace for `secs`: per thread, the busy share and its top events
    by total duration (complete `X` events, top level only)."""
    pg.trace, pg.trace_done = [], False
    await pg.cdp("Tracing.start", categories="devtools.timeline,disabled-by-default-devtools.timeline,toplevel,viz,cc,gpu", transferMode="ReportEvents")
    await asyncio.sleep(secs)
    await pg.cdp("Tracing.end")
    for _ in range(200):
        if pg.trace_done:
            break
        await pg.js("1")
        await asyncio.sleep(0.05)
    names = {(e.get("pid"), e.get("tid")): e["args"]["name"] for e in pg.trace if e.get("ph") == "M" and e.get("name") == "thread_name"}
    by: dict = {}
    for e in pg.trace:
        if e.get("ph") != "X" or "dur" not in e:
            continue
        th = names.get((e.get("pid"), e.get("tid")), str(e.get("tid")))
        d = by.setdefault(th, {})
        d[e["name"]] = d.get(e["name"], 0) + e["dur"] / 1000.0
    out = {}
    for th, d in by.items():
        top = sorted(d.items(), key=lambda x: -x[1])[:6]
        if top and top[0][1] > secs * 1000 * 0.05:
            out[th] = [f"{n}: {ms:.0f} ms" for n, ms in top]
    return out


async def run(rig: Rig, r: dict, info: dict) -> None:
    async with rig.page(1600, 1000, extra=["--use-angle=swiftshader", "--enable-unsafe-swiftshader"]) as pg:
        guid = rig.page_guid()
        ids = [c["id"] for c in seeds(N)[0]]
        if os.environ.get("LOAD_PROFILE") == "1":
            info["profile_boot"] = await profile(pg, 4.0)
        # every terminal up (its shell's prompt drawn)
        t0 = time.time()
        # xterm renders only what is ON SCREEN (an off-screen terminal is paused),
        # so "up" counts the terminals in the viewport
        VIS = "[...document.querySelectorAll('.agent-frame')].filter(f=>{const b=f.getBoundingClientRect(); return f.querySelector('.xterm') && b.right>0 && b.bottom>0 && b.left<innerWidth && b.top<innerHeight})"
        # the cells mount first (an empty set is not "all up"), then every one on screen draws its prompt
        # the boot, as a timeline: cells mounted every 5 s until all are (or it stalls)
        tl, t0b = [], time.time()
        while time.time() - t0b < 60 + N * 0.3:
            n = await pg.js("document.querySelectorAll('.agent-frame').length")
            smp = system_sample(rig.host.pid)
            tl.append(f"{time.time() - t0b:.0f}s:{n} cells" + (f", load {smp['load'][0]}, cpu {smp['cpu_total_pct']}%, gpu {smp['gpu_pct']}%" if smp else ""))
            if n >= N:
                break
            await asyncio.sleep(5)
        info["boot_timeline"] = tl
        info["console_errors"] = pg.logs[:10]
        await pg.until(f"{VIS}.length > 0 && {VIS}.every(f=>({TEXT})(f).trim().length>0)", 60 + N * 0.6)
        vis = await pg.js(f"{VIS}.length")
        up = await pg.js(f"{VIS}.filter(f=>({TEXT})(f).trim().length>0).length")
        info["renderer"] = await pg.js("(()=>{const c=document.querySelector('.agent-frame .xterm-screen canvas'); return c ? 'canvas' : 'dom'})()")
        info["terminals_up"] = f"{up}/{vis} on screen ({N} total) in {time.time() - t0:.1f}s"
        r["all_up"] = up == vis and vis > 0
        if os.environ.get("LOAD_NOBLUR") == "1":
            await pg.js("(()=>{const s=document.createElement('style'); s.textContent='.agent-frame,.frame-head{backdrop-filter:none!important;-webkit-backdrop-filter:none!important}'; document.head.appendChild(s)})()")
        # the PTY fit: each grid inside its visible area
        fit = await pg.js("""[...document.querySelectorAll('.agent-frame')].filter(f=>f.querySelector('.xterm-screen')).map(f=>{
            const b=f.querySelector('.frame-body'), x=f.querySelector('.xterm'), s=f.querySelector('.xterm-screen'), cs=getComputedStyle(x);
            return s.offsetWidth <= b.clientWidth-parseFloat(cs.paddingLeft)-parseFloat(cs.paddingRight)+0.5 && s.offsetHeight <= b.clientHeight-parseFloat(cs.paddingTop)-parseFloat(cs.paddingBottom)+0.5})""")
        r["pty_fit"] = bool(fit) and all(fit)
        info["pty_fit"] = f"{sum(1 for x in fit or [] if x)}/{len(fit or [])}"
        # the background
        from canvas import host_call
        if WITH_BG:
            bg = await host_call(rig.port, f"{guid}|background", {"type": "set_bg", "bytes": BG})
            r["bg_set"] = bool(bg.get("ok"))
        # the OUTPUT load: every terminal streams a line every 200 ms
        STREAM = os.environ.get("LOAD_STREAM", "1") != "0"
        info["stream"] = STREAM
        # the stream starts THROUGH EACH BACKEND (its live PTY job's `write`), not
        # by typing into N xterms — every terminal streams, deterministically
        async def stream(i: int) -> bool:
            bid = f"exec_bridge_load{i:04d}"
            st = await host_call(rig.port, bid, {"type": "status"})
            job = next((j.get("job_id") for j in st.get("jobs", []) if j.get("pty") and j.get("status") == "running"), None)
            if job is None:
                return False
            w = await host_call(rig.port, bid, {"type": "write", "job_id": job, "data": py_typed("ticker.py") + "\r"})
            return "error" not in w
        # at most 16 at a time: N connections at once overran the host's listen backlog
        gate = asyncio.Semaphore(16)
        async def gated(i: int) -> bool:
            async with gate:
                return await stream(i)
        started = await asyncio.gather(*(gated(i) for i in range(N))) if STREAM else []
        info["streams_started"] = f"{sum(started)}/{N}" if STREAM else "off"
        r["streams_started"] = (not STREAM) or all(started)
        await asyncio.sleep(2)
        if os.environ.get("LOAD_PROFILE") == "1":
            info["profile_load"] = await profile(pg, 4.0)
        if os.environ.get("LOAD_TRACE") == "1":
            info["trace"] = await trace(pg, 2.0)
        if os.environ.get("LOAD_EXPERIMENTS") in ("1", "webgl"):
            # one CSS suspect at a time, cumulative: the frame rate after each
            if os.environ.get("LOAD_EXPERIMENTS") == "webgl":
                res = []
                css = lambda c: f"(()=>{{const s=document.createElement('style'); s.textContent={json.dumps(c)}; document.head.appendChild(s)}})()"
                for name, js in [("baseline", ""),
                                 ("no scrollbars", css(".xterm .scrollbar,.xterm .slider,.xterm-decoration-overview-ruler{display:none!important}")),
                                 ("no frame chrome", css(".frame-head,.resize-handle{display:none!important}")),
                                 ("no link layer", css(".xterm-screen canvas:not(:first-child){display:none!important}"))]:
                    if js:
                        await pg.js(js)
                    await asyncio.sleep(3)
                    res.append(f"{name}: {await pg.js(fps_js0)} fps")
                info["experiments"] = res
            exps = [] if os.environ.get("LOAD_EXPERIMENTS") == "webgl" else [("baseline", ""),
                    ("frame own layer", ".agent-frame{will-change:transform}"),
                    ("rows own layer", ".xterm-rows{will-change:transform}"),
                    ("opaque terminal bg", ".xterm .xterm-viewport,.xterm .xterm-screen,.xterm-rows{background:#0b0b10!important}"),
                    ("no rounded clip", ".agent-frame{border-radius:0!important;overflow:visible!important}.frame-body{overflow:visible!important}"),
                    ("no cursor blink", ".xterm-cursor-blink,.xterm .xterm-cursor{animation:none!important}"),
                    ("no backdrop blur", ".agent-frame,.frame-head{backdrop-filter:none!important;-webkit-backdrop-filter:none!important}"),
                    ("no shadows/transitions", ".agent-frame{box-shadow:none!important;transition:none!important}"),
                    ("no world will-change", ".canvas-world{will-change:auto!important}"),
                    ("frames contained", ".agent-frame{contain:strict}")]
            res = []
            for name, css in exps:
                if css:
                    await pg.js(f"(()=>{{const s=document.createElement('style'); s.textContent={json.dumps(css)}; document.head.appendChild(s)}})()")
                await asyncio.sleep(1.5)
                res.append(f"{name}: {await pg.js(fps_js0)} fps")
            if exps:
                info["experiments"] = res
        # the page's WebSocket traffic: frames and bytes per second, each way
        pg.ws_frames = {"in": [0, 0], "out": [0, 0]}
        await pg.cdp("Network.enable")
        await asyncio.sleep(3.0)
        await pg.cdp("Network.disable")
        info["ws_per_s"] = {k: {"frames": round(v[0] / 3), "kb": round(v[1] / 3 / 1024, 1)} for k, v in pg.ws_frames.items()}
        # a stream that is not flowing measures nothing: ~5 lines/s per terminal
        # must reach the page (the off-screen ones are held, not dropped — they
        # still arrive)
        r["stream_flowing"] = (not STREAM) or info["ws_per_s"]["in"]["frames"] >= N * 3
        # THE WINDOW
        b0, w0, l0 = store_bytes(rig.proj), time.time(), len(rig.log.read_text().splitlines())
        fps_js = "new Promise(res=>{let n=0;const t=performance.now();const f=()=>{n++; if(performance.now()-t<1000) requestAnimationFrame(f); else res(n)}; requestAnimationFrame(f)})"
        lat, fps, guard, sys_samples = [], [], None, []
        # sample terminals ON SCREEN (an off-screen one is not drawn)
        onscreen = await pg.js(f"{VIS}.map(f=>f.dataset.id)") or ids
        sample = onscreen[:: max(1, len(onscreen) // SAMPLES)][:SAMPLES]
        ro0 = await pg.js("window.__ro ?? null")
        gl0 = await pg.js("({u: (globalThis.__fantasticWebgl||{}).uploads||0, px: (globalThis.__fantasticWebgl||{}).uploadPx||0, r: (globalThis.__fantasticWebgl||{}).renders||0})")
        while time.time() - w0 < SECONDS:
            if (store_bytes(rig.proj) - b0) / 1e6 > DISK_MB:
                guard = f"the store grew past {DISK_MB} MB — load stopped"
                break
            fps.append(await pg.js(fps_js))
            if (smp := system_sample(rig.host.pid)) is not None:
                sys_samples.append(smp)
            for cid in sample:
                # timed IN THE PAGE: a MutationObserver on that terminal's screen
                # stamps the moment the marker is drawn (no polling resolution)
                mk = "MK" + secrets.token_hex(3)
                await pg.js(ARM % (json.dumps(cid), json.dumps(mk)))
                await type_into(pg, cid, f": {mk}")
                ms = None
                for _ in range(500):
                    ms = await pg.js(f"window.__lat && window.__lat[{json.dumps(mk)}]")
                    if ms is not None:
                        break
                    await asyncio.sleep(0.01)
                lat.append(float(ms) if ms is not None else 5000.0)
        w1 = time.time()
        ro1 = await pg.js("window.__ro ?? null")
        gl1 = await pg.js("({u: (globalThis.__fantasticWebgl||{}).uploads||0, px: (globalThis.__fantasticWebgl||{}).uploadPx||0, r: (globalThis.__fantasticWebgl||{}).renders||0})")
        dt = time.time() - w0
        info["webgl_per_s"] = {"renders": round((gl1["r"] - gl0["r"]) / dt), "uploads": round((gl1["u"] - gl0["u"]) / dt, 1), "upload_mpx": round((gl1["px"] - gl0["px"]) / dt / 1e6, 1)}
        if ro0 is not None:
            info["resize_observer_per_s"] = round((ro1 - ro0) / (w1 - w0))
        b1, l1 = store_bytes(rig.proj), len(rig.log.read_text().splitlines())
        secs = w1 - w0
        rows = row_stats(rig.proj, w0, w1)
        lat.sort()
        info["load"] = {
            "n": N, "seconds": round(secs, 1), "disk_guard": guard,
            "echo_ms": {"p50": round(statistics.median(lat)), "p95": round(lat[int(len(lat) * 0.95) - 1]), "max": round(lat[-1]), "n": len(lat)} if lat else None,
            "fps": {"min": min(fps), "avg": round(statistics.mean(fps))} if fps else None,
            "store_mb_per_s": round((b1 - b0) / 1e6 / secs, 3),
            "store_mb_total": round(b1 / 1e6, 1),
            "rows_per_s": round(rows["rows"] / secs),
            "rows_by_kind_per_s": {k: round(v / secs, 1) for k, v in rows["by_kind"].items()},
            "top_calls": rows["top_calls"], "top_settled": rows["top_settled"],
            "host_lines_per_s": round((l1 - l0) / secs),
            "system": sys_samples[len(sys_samples) // 2] if sys_samples else None,
            "system_peak": {"load": max(x["load"][0] for x in sys_samples), "cpu_total_pct": max(x["cpu_total_pct"] for x in sys_samples),
                            "gpu_pct": max((x["gpu_pct"] or 0) for x in sys_samples)} if sys_samples else None,
        }
        r["no_disk_guard"] = guard is None
        # the kernel's execution rows are RAM-only (`kernel_memory`, owner
        # 2026-10-09): none of the load's calls may reach memory.db
        r["no_kernel_rows_on_disk"] = rows["rows"] == 0
        r["echoes_seen"] = bool(lat) and lat[-1] < 5000


def main() -> bool:
    rig = Rig(__file__, live=False)
    rig.build()
    rig.make_home(None)
    cells, hosts = seeds(N)
    # the stream each terminal runs: a timestamp every 20 ms, in any shell
    rig.proj.mkdir(parents=True, exist_ok=True)
    (rig.proj / "ticker.py").write_text("import time\nwhile True:\n    print(f'{time.time():.6f}', flush=True)\n    time.sleep(0.02)\n")
    r: dict = {}
    info: dict = {"n": N, "bg": WITH_BG}
    try:
        rig.init(cells=cells, host_agents=hosts)
        asyncio.run(run(rig, r, info))
    finally:
        rig.stop()
    print(json.dumps(info, indent=1))
    ok = all(r.values())
    rig.summary["checks"] = r
    rig.summary["info"] = info
    failed = [k for k, v in r.items() if not v]
    return verdict(*rig.finish(ok, "measured" if ok else f"failed: {', '.join(failed)}"))


if __name__ == "__main__":
    sys.exit(0 if main() else 1)
