"""E60 bundle_flow — a python agent's whole life on the canvas (a → f): made from
the double-click, edited live, kept across a restart, bundled into a release that
lives on its own, and updated by an AI — which must tell the BUNDLE (code, every
instance) from the AGENT (an instance)."""

from __future__ import annotations

import asyncio
import json
import re
import shutil
import subprocess
import sys
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[2]))
from _lib.canvas_live import Rig, verdict, ask, say, wait_quiet, saved_dir  # noqa: E402

RELEASE = "release_agent"


def main() -> bool:
    rig = Rig(__file__)
    why = rig.plug_or_refusal()
    if why:
        return verdict(*rig.finish(False, why))
    rig.build()
    rig.make_home(rig.plug.record())
    rig.init()
    from _lib.dev_surface import frame_eval

    bundles = rig.proj / ".fantastic" / "bundles"

    def send(target: str, payload: dict) -> dict:
        """One call into the running kernel from the shell (`fantastic send`)."""
        r = subprocess.run([str(rig.binary), "send", json.dumps({"target": target, "payload": payload})],
                           cwd=rig.proj, env=rig.env(), capture_output=True, text=True, timeout=60)
        try:
            return json.loads(r.stdout).get("result") or {}
        except ValueError:
            return {"error": (r.stdout + r.stderr)[-300:]}

    def hello(agent: str) -> str:
        return str(send(agent, {"type": "hello"}).get("greeting", ""))

    async def until(pred, secs: float) -> bool:
        for _ in range(int(secs * 2)):
            if pred():
                return True
            await asyncio.sleep(0.5)
        return pred()

    async def panel_text(pg) -> str:
        return "\n".join(v for v in await frame_eval(pg, "document.body ? document.body.innerText : ''") if isinstance(v, str))

    def edit(path: Path, old: str, new: str) -> None:
        path.write_text(path.read_text().replace(old, new))

    r: dict = {}
    info: dict = {}

    async def drive() -> None:
        async with rig.page(1900, 1100) as pg:
            # a) the AI is set up: its saved set-up is the default (its cell opens on the first ask, f)
            r["ai_ready"] = (saved_dir(rig.home) / ".preferred").exists()
            # b) double-click → python agent
            for c in (1, 2):
                await pg.mouse("mousePressed", 900, 500, c)
                await pg.mouse("mouseReleased", 900, 500, c)
            await pg.until("!!document.querySelector('.action-popup input')", 10)
            await pg.js("(()=>{const t=[...document.querySelectorAll('.action-popup .tile')].find(e=>(e.querySelector('.lb')||{}).textContent==='python agent'); if(t) t.dispatchEvent(new MouseEvent('mousedown',{bubbles:true}))})()")
            made = await until(lambda: any(bundles.glob("py_*/agent.py")), 20)
            r["bundle_made"] = made
            dev = next(bundles.glob("py_*/agent.py")).parent.name if made else ""
            inst = f"{dev}_1"
            info["dev"] = dev
            r["instance_made"] = made and await until(lambda: rig.host_record(inst) is not None, 15)
            r["view_shown"] = r["instance_made"] and bool(await pg.until(
                "[...document.querySelectorAll('.agent-frame')].some(f=>f.dataset.side==='host' && (f.querySelector('iframe')||{}).srcdoc && f.querySelector('iframe').srcdoc.includes('python agent'))", 20))
            r["hello_works"] = r["instance_made"] and await until(lambda: hello(inst) == "hello, world", 20)
            # c) edit the logic, then the view — each takes effect at once
            if r["hello_works"]:
                edit(bundles / dev / "agent.py", 'f"hello, {', 'f"hi there, {')
                r["logic_live"] = await until(lambda: hello(inst) == "hi there, world", 20)
                edit(bundles / dev / "panel.html", "<h3>python agent</h3>", "<h3>python agent</h3><p>view-two</p>")
                r["view_live"] = await until(lambda: False, 0) or bool(await pg.until(
                    "[...document.querySelectorAll('.agent-frame iframe')].some(i=>(i.srcdoc||'').includes('view-two'))", 15))
            # e) bundle it: a release that lives its own life
            if r.get("logic_live"):
                shutil.copytree(bundles / dev, bundles / RELEASE, ignore=shutil.ignore_patterns("__pycache__"))
                loaded = await until(lambda: "error" not in send("kernel", {"type": "create_agent", "parent_id": "canvas_root", "id": f"{RELEASE}_1", "handler_module": RELEASE}) or rig.host_record(f"{RELEASE}_1") is not None, 20)
                r["release_loaded"] = loaded and await until(lambda: hello(f"{RELEASE}_1") == "hi there, world", 20)
                if r["release_loaded"]:
                    edit(bundles / dev / "agent.py", 'f"hi there, {', 'f"dev-three, {')
                    changed = await until(lambda: hello(inst) == "dev-three, world", 20)
                    r["independent"] = changed and hello(f"{RELEASE}_1") == "hi there, world"
        # d) restart the host: the instance comes back
        rig.stop()
        rig.start()
        r["hydrated"] = await until(lambda: hello(f"{info.get('dev')}_1") in ("dev-three, world", "hi there, world"), 30)
        # f) the AI — the bundle (code) vs the agent (an instance)
        if r.get("independent") and r["hydrated"]:
            async with rig.page(1900, 1100) as pg:
                dev = info["dev"]
                done1, said1 = await ask(pg, 250, 300,
                    f"In the bundle `{RELEASE}` (.fantastic/bundles/{RELEASE}/agent.py) make hello greet with 'from-release' instead of 'hi there'. Do not touch the bundle `{dev}`.",
                    rig.budget(rig.limits["ask_seconds"], reserve=rig.limits["ask_seconds"] + 30), rig.limits["quiet_seconds"])
                r["ai_updates_release"] = await until(lambda: hello(f"{RELEASE}_1") == "from-release, world", 20) and hello(f"{dev}_1") == "dev-three, world"
                await say(pg, f"Now overwrite the bundle `{RELEASE}` with the code of the bundle `{dev}` (its agent.py).")
                done2 = await wait_quiet(pg, rig.budget(rig.limits["ask_seconds"], reserve=20), rig.limits["quiet_seconds"])
                r["ai_overwrites_release"] = await until(lambda: hello(f"{RELEASE}_1") == "dev-three, world", 20)
                info.update({"said1": said1, "turn1_quiet": done1, "turn2_quiet": done2})

    try:
        asyncio.run(drive())
    finally:
        rig.stop()
    order = ["ai_ready", "bundle_made", "instance_made", "view_shown", "hello_works", "logic_live", "view_live",
             "release_loaded", "independent", "hydrated", "ai_updates_release", "ai_overwrites_release"]
    checks = {k: bool(r.get(k)) for k in order}
    rig.summary.update({"checks": checks, **info})
    return verdict(*rig.finish(all(checks.values()), " ".join(f"{k}={'✓' if v else '✗'}" for k, v in checks.items())))


if __name__ == "__main__":
    sys.exit(0 if main() else 1)
