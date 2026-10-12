"""E56 dev_reactive — the reactive check on a hand-wired reference surface (no model)."""

from __future__ import annotations

import asyncio
import re
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[2]))
from _lib.canvas_live import FOLDER, Rig, verdict, answer, ask, html_cell, http_get, in_frame, say, spot, wait_quiet  # noqa: E402,F401

def surface(port: int) -> list[dict]:
    """The REFERENCE dev surface, wired by hand: what E55 asks an AI to build."""
    grant = {"dev": ["start", "stop", "status", "watch", "reflect"]}
    run_args = ["-m", "http.server", str(port), "--directory", "dist"]
    controls = f"""<h3>Controls</h3><button onclick="go('build')">Build</button> <button onclick="go('run')">Run</button> <button onclick="go('stop')">Stop</button>
<script>let runJob=null;
async function go(w){{
  if(w==='build'){{const r=await fantastic.send('dev',{{type:'start',command:'sh',args:['build.sh']}});fantastic.emit('buildlog',{{type:'job',job_id:r&&r.job_id}});fantastic.emit('status',{{type:'state',s:'building'}});}}
  if(w==='run'){{const r=await fantastic.send('dev',{{type:'start',command:'python3',args:{run_args!r}}});runJob=r&&r.job_id;fantastic.emit('runlog',{{type:'job',job_id:runJob}});fantastic.emit('status',{{type:'state',s:'running'}});setTimeout(()=>fantastic.emit('app',{{type:'reload'}}),1200);}}
  if(w==='stop'&&runJob!==null){{await fantastic.send('dev',{{type:'stop',job_id:runJob}});}}
}}</script>"""
    log = lambda title: f"""<h3>{title}</h3><pre id=l></pre><script>let job=null;
fantastic.onMessage(p=>{{if(p&&p.type==='job')job=p.job_id}});
fantastic.watch('dev',ev=>{{if(!ev||ev.job_id!==job)return; if(ev.type==='progress')document.getElementById('l').textContent+=ev.line+'\\n'; if(ev.type==='job_done')document.getElementById('l').textContent+='[exit '+ev.exit_code+']\\n';}});</script>"""
    status = """<h3>Status</h3><p id=s>idle</p><script>
fantastic.onMessage(p=>{if(p&&p.type==='state')document.getElementById('s').textContent=p.s});
fantastic.watch('dev',ev=>{if(!ev)return; if(ev.type==='progress'&&/^built:/.test(ev.line))document.getElementById('s').textContent='built';
 if(ev.type==='job_done'&&document.getElementById('s').textContent==='running')document.getElementById('s').textContent='stopped';});</script>"""
    app = f"""<h3>App</h3><iframe id=f style="width:100%;height:120px" src="about:blank"></iframe><script>
fantastic.onMessage(p=>{{if(p&&p.type==='reload')document.getElementById('f').src='http://127.0.0.1:{port}/?'+Date.now()}});</script>"""
    wires = lambda to: {"can_call": {**grant, to: ["emit"]}} if to else {"can_call": grant}
    return [
        html_cell("controls", "Controls", 700, 40, controls, 360, 150, can_call={**grant, "buildlog": ["emit"], "runlog": ["emit"], "status": ["emit"], "app": ["emit"]}),
        html_cell("buildlog", "Build log", 1100, 40, log("Build log"), 360, 220, can_call=grant, callable_by={"controls": ["emit"]}),
        html_cell("runlog", "Run log", 1500, 40, log("Run log"), 360, 220, can_call=grant, callable_by={"controls": ["emit"]}),
        html_cell("status", "Status", 700, 250, status, 360, 150, can_call=grant, callable_by={"controls": ["emit"]}),
        html_cell("app", "App", 1100, 300, app, 420, 220, callable_by={"controls": ["emit"]}),
    ]


def main() -> bool:
    from _lib.canvas_live import free_port
    from _lib.dev_surface import exec_record, reactive_check, write_project
    rig = Rig(__file__, live=False)
    rig.build()
    rig.make_home(None)
    port = free_port()
    write_project(rig.proj)
    rig.init(host_agents=[(FOLDER, exec_record(port))])
    rig.stop()
    rig.seed_cells(surface(port))
    rig.start()

    async def drive():
        async with rig.page(1900, 1100) as pg:
            await pg.until("document.querySelectorAll('.agent-frame iframe').length >= 5", 20)
            await asyncio.sleep(2)
            return await reactive_check(pg, rig.proj, port)
    try:
        checks = asyncio.run(drive())
    finally:
        rig.stop()
    rig.summary.update({"checks": checks})
    print(f"reactive checks: {checks}")
    return verdict(*rig.finish(all(checks.values()), " ".join(f"{k}={'✓' if v else '✗'}" for k, v in checks.items())))


if __name__ == "__main__":
    sys.exit(0 if main() else 1)
