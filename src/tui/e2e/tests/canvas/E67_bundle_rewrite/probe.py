"""E67 bundle_rewrite — the owner's tip-3 run, scripted (2026-10-09: a coding
agent made a python agent, rewrote it, bundled it — and its panel stayed the
starter's, calling verbs that were gone; the bundle was not placeable):

a) double-click → `python agent` places `py_<hex>_1` with the starter's panel;
b) ONE save rewrites agent.py, bundle.yaml (its verbs: `scan` only) and
   panel.html (a button calling `scan`) — as an agent rewriting a bundle does;
c) the placed panel re-renders to the NEW body, its cell is granted `scan`, and a
   click on the panel's button reaches `scan` on the host;
d) the folder copied to a new name (`dash_release`) is a tile in the popup's
   `host · bundles` group;
e) that tile places `dash_release_1`, its panel the new one, its button working."""

from __future__ import annotations

import asyncio
import json
import re
import shutil
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[2]))
from _lib.canvas_live import Rig, spot, verdict  # noqa: E402

RELEASE = "dash_release"
AGENT_PY = '''import fantastic_agent as fa


def scan(p):
    return {"files": 2, "lines": 46}


fa.serve({"scan": scan})
'''
BUNDLE_YAML = '''base: py_agent
sentence: A project dashboard (E67) — one verb, scan.
command: [python3, agent.py]
verbs:
  scan: "args: none. Returns {files, lines}."
record:
  inherit_env: [PATH]
  view: {kind: html, bytes_file: panel.html, calls: [scan, watch]}
'''
# the button fills the panel's top-left: a click at a known spot
PANEL = '''<style>body{margin:0} #go{position:fixed;left:0;top:0;width:220px;height:60px}</style>
<button id=go>SCAN-E67</button><pre id=out></pre>
<script>
const AGENT = "{{agent}}";
document.getElementById("go").onclick = async () => { const r = await fantastic.send(AGENT, {type: "scan"}); document.getElementById("out").textContent = JSON.stringify(r); };
</script>
'''


def calls_to(rig: Rig, agent: str, verb: str) -> int:
    return sum(1 for l in rig.log.read_text().splitlines() if re.match(rf"^\[\d+\] {re.escape(agent)} call {verb}\b", l))


async def panel_of(pg, cell: str) -> str:
    return await pg.js(f"document.querySelector('.agent-frame[data-id={json.dumps(cell)}] iframe')?.srcdoc || ''") or ""


async def click_panel(pg, cell: str) -> None:
    r = await pg.rect(f".agent-frame[data-id={json.dumps(cell)}] iframe")
    x, y = r["x"] + 40, r["y"] + 25
    # point at the cell first (its chrome shows on hover), then click the button
    await pg.cdp("Input.dispatchMouseEvent", type="mouseMoved", x=x, y=y)
    await pg.mouse("mousePressed", x, y)
    await pg.mouse("mouseReleased", x, y)


def cell_of(rig: Rig, backend: str) -> dict:
    return next((c for c in rig.cells() if c.get("backend_id") == backend), {})


def main() -> bool:
    rig = Rig(__file__, live=False)
    rig.build()
    rig.make_home(None)
    r: dict = {}
    info: dict = {}
    try:
        rig.init()

        async def run() -> None:
            async with rig.page(1500, 950) as pg:
                bundles = rig.proj / ".fantastic" / "bundles"
                # a) the starter, placed
                await spot(pg, 300, 260, "python agent")
                ok = await pg.until("[...document.querySelectorAll('.agent-frame iframe')].some(f=>(f.srcdoc||'').includes('python agent'))", 30)
                folder = next((d for d in bundles.iterdir() if d.name.startswith("py_")), None) if bundles.is_dir() else None
                r["starter_placed"] = bool(ok) and folder is not None
                if folder is None:
                    return
                name = folder.name
                inst = f"{name}_1"
                cell = await pg.js("[...document.querySelectorAll('.agent-frame')].find(f=>(f.querySelector('iframe')?.srcdoc||'').includes('python agent'))?.dataset.id") or ""
                info["bundle"], info["instance"], info["cell"] = name, inst, cell
                # b) ONE save: code, verbs and panel together
                (folder / "agent.py").write_text(AGENT_PY)
                (folder / "bundle.yaml").write_text(BUNDLE_YAML)
                (folder / "panel.html").write_text(PANEL)
                # c) the placed panel is the new one, granted `scan`, and its click reaches it
                r["panel_rerendered"] = bool(await pg.until(
                    f"(document.querySelector('.agent-frame[data-id={json.dumps(cell)}] iframe')?.srcdoc||'').includes('SCAN-E67')", 20))
                await asyncio.sleep(1.0)
                grant = ((next((c for c in rig.cells() if c.get("id") == cell), {}).get("can_call")) or {}).get(inst) or []
                info["cell_grant"] = grant
                r["panel_granted_new_verb"] = "scan" in grant and "hello" not in grant
                before = calls_to(rig, inst, "scan")
                await click_panel(pg, cell)
                await asyncio.sleep(2.0)
                r["panel_click_reaches_new_verb"] = calls_to(rig, inst, "scan") > before
                # d) bundled under a new name → a popup tile
                shutil.copytree(folder, bundles / RELEASE, ignore=shutil.ignore_patterns("__pycache__"))
                await asyncio.sleep(2.0)
                for c in (1, 2):
                    await pg.mouse("mousePressed", 900, 200, c)
                    await pg.mouse("mouseReleased", 900, 200, c)
                await pg.until("!!document.querySelector('.action-popup .tile')", 10)
                tiles = await pg.js("[...document.querySelectorAll('.action-popup .tile[data-section=bundles] .lb')].map(e=>e.textContent)")
                info["bundle_tiles"] = tiles
                r["release_is_a_tile"] = RELEASE in (tiles or [])
                await pg.cdp("Input.dispatchKeyEvent", type="keyDown", key="Escape", code="Escape", windowsVirtualKeyCode=27)
                await asyncio.sleep(0.4)
                # e) placed from the popup, working
                await spot(pg, 900, 200, RELEASE)
                rel = f"{RELEASE}_1"
                placed = await pg.until(f"!!document.querySelector('.agent-frame iframe') && [...document.querySelectorAll('.agent-frame iframe')].filter(f=>(f.srcdoc||'').includes('SCAN-E67')).length >= 2", 30)
                rcell = await pg.js(f"[...document.querySelectorAll('.agent-frame')].map(f=>f.dataset.id).find(id=>id!=={json.dumps(cell)} && (document.querySelector('.agent-frame[data-id='+JSON.stringify(id)+'] iframe')?.srcdoc||'').includes('SCAN-E67'))") or ""
                info["release_cell"] = rcell
                r["release_placed"] = bool(placed) and rcell != ""
                if rcell:
                    await asyncio.sleep(1.0)  # its panel's script attached, as in (c)
                    before = calls_to(rig, rel, "scan")
                    await click_panel(pg, rcell)
                    await asyncio.sleep(2.0)
                    r["release_click_works"] = calls_to(rig, rel, "scan") > before
                info["page_errors"] = pg.logs[:5]

        asyncio.run(run())
    finally:
        rig.stop()
    print(json.dumps(info, indent=1))
    ok = bool(r) and all(r.values())
    rig.summary["checks"] = r
    rig.summary["info"] = info
    failed = [k for k, v in r.items() if not v]
    return verdict(*rig.finish(ok, "all checks pass" if ok else f"failed: {', '.join(failed)}"))


if __name__ == "__main__":
    sys.exit(0 if main() else 1)
