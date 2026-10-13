"""Run the canvas user scenario (E21) — headless Chromes over CDP, no model.

    cd src/lib/python && .venv/bin/python -u ../../../e2e/runners/canvas_user.py

One host, ONE user (owner, 2026-10-03: the multi-user paradigm is dropped): a tab
gets in through `user`, and `around` reads the page records.
"""

from __future__ import annotations

import asyncio
import json
import os
import shutil
import subprocess
import sys
import tempfile
import time
import urllib.request

import yaml
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from canvas import MAC_CHROME, Host, Page, build_bin, free_port, host_call, yaml_of  # noqa: E402

E2E = next(d for d in Path(__file__).resolve().parents if (d / ".e2e-root").exists())
sys.path.insert(0, str(E2E))
from helpers import verdict  # noqa: E402

SC = E2E / "tests" / "scenario" / "E21_canvas_user" / "scenario.yaml"
FRAMES = "document.querySelectorAll('#world .agent-frame, .agent-frame').length"


class Browser:
    """One Chrome = one identity (its own storage)."""

    def __init__(self, tmp: Path, name: str, w: dict):
        self.port = free_port()
        chrome = os.environ.get("CHROME") or MAC_CHROME
        if not Path(chrome).exists():
            raise SystemExit(f"no Chrome at {chrome}: set CHROME")
        self.proc = subprocess.Popen([chrome, "--headless=new", "--use-angle=swiftshader", "--enable-unsafe-swiftshader", f"--window-size={w['width']},{w['height']}",
                                      f"--remote-debugging-port={self.port}", f"--user-data-dir={tmp / ('chrome-' + name)}",
                                      "--no-first-run", "about:blank"], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        self.used_first = False

    def ws_url(self) -> str:
        deadline = time.time() + 15
        while True:
            try:
                tabs = [t for t in json.loads(urllib.request.urlopen(f"http://127.0.0.1:{self.port}/json/list").read()) if t["type"] == "page"]
                if not self.used_first and tabs:
                    self.used_first = True
                    return tabs[0]["webSocketDebuggerUrl"]
                t = json.loads(urllib.request.urlopen(urllib.request.Request(f"http://127.0.0.1:{self.port}/json/new?about:blank", method="PUT")).read())
                return t["webSocketDebuggerUrl"]
            except Exception:
                if time.time() > deadline:
                    raise SystemExit("Chrome never opened its debugging port")
                time.sleep(0.2)

    def close(self) -> None:
        self.proc.terminate()
        try:
            self.proc.wait(10)
        except subprocess.TimeoutExpired:
            self.proc.kill()


async def tab(b: Browser, url: str, *, stack) -> Page:
    import websockets

    ws = await stack.enter_async_context(websockets.connect(b.ws_url(), max_size=None))
    p = Page(ws)
    await p.cdp("Runtime.enable")
    await p.cdp("Page.enable")
    await p.open(url)
    return p


async def ready(p: Page) -> bool:
    # nothing floats (owner 2026-10-08): the canvas is drawn once its world is
    return await p.until("!!document.getElementById('canvas') && !!document.querySelector('.canvas-world')", 20)


async def place(p: Page, kind: str, x: int, y: int) -> bool:
    await p.js(f"document.getElementById('canvas').dispatchEvent(new MouseEvent('dblclick',{{bubbles:true,clientX:{x},clientY:{y}}}))")
    if not await p.until("!!document.querySelector('.action-popup .tile')", 5):
        return False
    await p.js(f"document.querySelector('.action-popup .tile[data-kind={kind}]').dispatchEvent(new MouseEvent('mousedown',{{bubbles:true}}))")
    return True


def members(cwd: Path, under: str = "view_root") -> list[str]:
    """The cells of page view `under` (default: the root view) on disk — its
    background is not a cell, nor a double click's ephemeral selector."""
    d = cwd / ".fantastic" / "web" / "agents" / "canvas" / "agents" / "view_root" / "agents"
    if under != "view_root":
        d = d / under / "agents"
    return sorted(f.parent.name for f in d.glob("*/agent.yaml")
                  if (yaml.safe_load(f.read_text()) or {}).get("handler_module") not in ("background.ts", "selector.ts")) if d.is_dir() else []


async def wait_for(cond, timeout: float = 8.0) -> bool:
    deadline = time.time() + timeout
    while time.time() < deadline:
        if cond():
            return True
        await asyncio.sleep(0.2)
    return False


async def run(sc: dict, tmp: Path) -> bool:
    from contextlib import AsyncExitStack

    cwd = tmp / "work"
    cwd.mkdir(parents=True)
    binary = build_bin(tmp)
    host = Host(binary, cwd, free_port())
    browsers: list[Browser] = []
    results: list[tuple[str, list[str]]] = []
    try:
        host.start()
        url = host.url
        assert url is not None
        port = host.port
        async with AsyncExitStack() as stack:
            me = Browser(tmp, "me", sc["window"])
            browsers.append(me)

            # ── a tab gets in ───────────────────────────────────────────────
            bad: list[str] = []
            a = await tab(me, url, stack=stack)
            if not await ready(a):
                bad.append(f"tab A never showed the canvas: {await a.js('document.body.innerText.slice(0,200)')}")
            if not await place(a, "html", 500, 300):
                bad.append("no action popup on a dblclick in tab A")
            if not await a.until(f"{FRAMES} === 1", 8):
                bad.append(f"tab A shows {await a.js(FRAMES)} cells, want 1")
            if not await wait_for(lambda: len(members(cwd)) >= 1):
                bad.append(f"no cell on disk: {members(cwd)}")
            results.append(("tab-gets-in", bad))

            # `around` is the host's, from the page records (a tab: as `user`)
            bad = []
            r = await host_call(port, "user", {"type": "around", "x": 0, "y": 0, "radius": 5000})
            near = [m["id"] for m in r.get("members", [])]
            if not near:
                bad.append(f"around on the root view found nothing: {r}")
            results.append(("around-from-records", bad))

            for p in (a,):
                for l in p.logs:
                    if "EXC" in l:
                        results.append(("page exceptions", [l]))
    finally:
        if host.proc is not None:
            host.stop()
        for br in browsers:
            br.close()
    for name, bad in results:
        print(f"  {'✓' if not bad else '✗'} {name}", flush=True)
        for x in bad:
            print(f"      {x}", flush=True)
    return all(not bad for _, bad in results)


def main() -> int:
    import yaml

    sc = yaml.safe_load(SC.read_text())
    print(f"E21_{sc['id']} · {sc['title']}", flush=True)
    tmp = Path(tempfile.mkdtemp(prefix="e2e-user-"))
    try:
        ok = asyncio.run(run(sc, tmp))
    finally:
        if not os.environ.get("E2E_KEEP"):
            shutil.rmtree(tmp, ignore_errors=True)
        else:
            print(f"  kept {tmp}")
    verdict(ok, "all runs green" if ok else "failures above")
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main())
