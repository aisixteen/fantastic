"""Run the canvas users scenario (E21) — headless Chromes over CDP, no model.

    cd src/lib/python && .venv/bin/python -u ../../../e2e/runners/canvas_users.py

One host; one Chrome per IDENTITY (a browser keeps one token per origin, in its
storage); every check on the page AND through the gateway.
"""

from __future__ import annotations

import asyncio
import hashlib
import json
import os
import shutil
import subprocess
import sys
import tempfile
import time
import urllib.request
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from canvas import MAC_CHROME, Host, Page, build_bin, free_port, host_call, yaml_of  # noqa: E402

E2E = next(d for d in Path(__file__).resolve().parents if (d / ".e2e-root").exists())
sys.path.insert(0, str(E2E))
from helpers import verdict  # noqa: E402

SC = E2E / "tests" / "scenario" / "E21_canvas_users" / "scenario.yaml"
FRAMES = "document.querySelectorAll('#world .agent-frame, .agent-frame').length"


def sha(t: str) -> str:
    return hashlib.sha256(t.encode()).hexdigest()


class Browser:
    """One Chrome = one identity (its own storage)."""

    def __init__(self, tmp: Path, name: str, w: dict):
        self.port = free_port()
        chrome = os.environ.get("CHROME") or MAC_CHROME
        if not Path(chrome).exists():
            raise SystemExit(f"no Chrome at {chrome}: set CHROME")
        self.proc = subprocess.Popen([chrome, "--headless=new", "--disable-gpu", f"--window-size={w['width']},{w['height']}",
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


async def tab(b: Browser, url: str, token: str | None = None, *, stack) -> Page:
    import websockets

    ws = await stack.enter_async_context(websockets.connect(b.ws_url(), max_size=None))
    p = Page(ws)
    await p.cdp("Runtime.enable")
    await p.cdp("Page.enable")
    await p.open(url)
    if token is not None:
        await p.js(f"localStorage.setItem('fantastic.canvas.token', {json.dumps(token)})")
        await p.open(url)
    return p


async def ready(p: Page) -> bool:
    return await p.until("!!document.getElementById('canvas') && !!document.querySelector('#toolbar')", 20)


async def place(p: Page, kind: str, x: int, y: int) -> bool:
    await p.js(f"document.getElementById('canvas').dispatchEvent(new MouseEvent('dblclick',{{bubbles:true,clientX:{x},clientY:{y}}}))")
    if not await p.until("!!document.querySelector('.action-popup .tile')", 5):
        return False
    await p.js(f"document.querySelector('.action-popup .tile[data-kind={kind}]').dispatchEvent(new MouseEvent('mousedown',{{bubbles:true}}))")
    return True


def members(cwd: Path, under: str = "canvas") -> list[str]:
    d = cwd / ".fantastic" / "web" / "agents" / "canvas" / "agents"
    if under != "canvas":
        d = d / under / "agents"
    return sorted(f.parent.name for f in d.glob("*/agent.yaml")) if d.is_dir() else []


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
    tok = sc["tokens"]
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

            # ── default: no token = full rights ─────────────────────────────
            bad: list[str] = []
            a = await tab(me, url, stack=stack)
            if not await ready(a):
                bad.append(f"tab A never showed the canvas: {await a.js('document.body.innerText.slice(0,200)')}")
            b = await tab(me, url, stack=stack)
            await ready(b)
            if not await place(a, "html", 500, 300):
                bad.append("no action popup on a dblclick in tab A")
            if not await a.until(f"{FRAMES} === 1", 8):
                bad.append(f"tab A shows {await a.js(FRAMES)} cells, want 1")
            if not await wait_for(lambda: len(members(cwd)) >= 1):
                bad.append(f"no cell on disk: {members(cwd)}")
            if await a.js("document.body.classList.contains('ro')"):
                bad.append("tab A is locked with no token")
            results.append(("default-full-rights", bad))

            # ── two tabs, live ──────────────────────────────────────────────
            bad = []
            if not await b.until(f"{FRAMES} === 1", 10):
                bad.append(f"tab B shows {await b.js(FRAMES)} cells without a reload, want 1")
            await a.js("document.querySelector('.agent-frame .close').click()")
            if not await b.until(f"{FRAMES} === 0", 10):
                bad.append(f"tab B still shows {await b.js(FRAMES)} cells after tab A closed it")
            results.append(("two-tabs-live", bad))

            # the tree for the users: a cell on the root, a child canvas with none
            await place(a, "html", 300, 250)
            await place(a, "canvas", 800, 400)
            await wait_for(lambda: len(members(cwd)) >= 2)
            root = yaml_of(cwd / ".fantastic" / "web" / "agents" / "canvas" / "agent.yaml")
            guid = root.get("guid", "")
            child = next((m for m in members(cwd) if m.startswith("canvas_")), "")
            # USERS ARE THE HUMAN'S to write — as the operator: their records on
            # disk, then a restart (a non-operator cannot delegate page grants it
            # holds only through `*|*`)
            import yaml

            users_dir = cwd / ".fantastic" / "agents" / "set_of_agents" / "agents" / "users" / "agents"
            for uid, cc in (("ro", {f"{guid}|canvas": ["reflect"]}), ("ed", {f"{guid}|{child}": ["*"]})):
                d = users_dir / uid
                d.mkdir(parents=True, exist_ok=True)
                (d / "agent.yaml").write_text(yaml.safe_dump({"id": uid, "handler_module": "canvas_user.tools",
                                                              "token_sha256": sha(tok[uid]), "can_call": cc, "parent_id": "users"}))
            host.stop()
            host.start()
            if not await a.until(f"{FRAMES} >= 2", 15):
                results.append(("setup", [f"tab A lost its cells over the restart: {await a.js(FRAMES)}"]))

            # ── read-only ───────────────────────────────────────────────────
            bad = []
            ro = Browser(tmp, "ro", sc["window"])
            browsers.append(ro)
            c = await tab(ro, url, tok["ro"], stack=stack)
            if not await ready(c):
                bad.append(f"the ro tab never showed the canvas: {await c.js('document.body.innerText.slice(0,200)')}")
            n_a = await a.js(FRAMES)
            if not await c.until(f"{FRAMES} === {n_a}", 8):
                bad.append(f"ro sees {await c.js(FRAMES)} cells, tab A {n_a}")
            if not await c.js("document.body.classList.contains('ro')"):
                bad.append("the ro tab is not locked")
            await c.js("document.getElementById('canvas').dispatchEvent(new MouseEvent('dblclick',{bubbles:true,clientX:400,clientY:600}))")
            await asyncio.sleep(1)
            if await c.js("!!document.querySelector('.action-popup')"):
                bad.append("the ro tab opened the action popup")
            r = await host_call(port, "web_loader", {"type": "persist_record", "record": {"id": "x1", "parent_id": "canvas", "handler_module": "html_agent.ts"}}, tok["ro"])
            if r.get("reason") != "unauthorized":
                bad.append(f"ro wrote a page record: {r}")
            results.append(("read-only-user", bad))

            # ── partial ─────────────────────────────────────────────────────
            bad = []
            ed = Browser(tmp, "ed", sc["window"])
            browsers.append(ed)
            d = await tab(ed, url, tok["ed"], stack=stack)
            await ready(d)
            if not await d.js("document.body.classList.contains('ro')"):
                bad.append("ed may not write the root canvas, but it is not locked")
            await d.open(url + f"#view={child}")
            await ready(d)
            if not await d.until("!document.body.classList.contains('ro')", 8):
                bad.append(f"ed's own canvas {child} is locked")
            before = members(cwd, child)
            if not await place(d, "html", 500, 300):
                bad.append("no action popup on ed's canvas")
            if not await wait_for(lambda: len(members(cwd, child)) == len(before) + 1):
                bad.append(f"ed's cell is not on disk under {child}: {members(cwd, child)}")
            results.append(("partial-user", bad))

            # ── partial sees only its canvas ────────────────────────────────
            bad = []
            r = await host_call(port, "web_loader", {"type": "load_tree"}, tok["ed"])
            ids = sorted(x["id"] for x in r.get("records", []))
            root_cells = [m for m in members(cwd) if not m.startswith("canvas_") and not m.endswith("_bg") and m != "background"]
            leaked = [m for m in root_cells if m in ids]
            if leaked:
                bad.append(f"ed sees the root's other cells: {leaked}")
            if child not in ids or "canvas" not in ids:
                bad.append(f"ed's tree lacks its canvas or the path to it: {ids}")
            # `around` is the host's, from the page records, judged by the caller's grants
            r = await host_call(port, "users", {"type": "around", "x": 0, "y": 0, "radius": 5000})
            near = sorted(m["id"] for m in r.get("members", []))
            if child not in near or not any(m in near for m in root_cells):
                bad.append(f"the default's around misses cells on the root: {near} (root cells {root_cells}, child {child})")
            r = await host_call(port, "users", {"type": "around", "x": 0, "y": 0, "radius": 5000}, tok["ed"])
            near_ed = sorted(m["id"] for m in r.get("members", []))
            if near_ed != [child]:
                bad.append(f"ed's around on the root shows {near_ed}, want only its canvas [{child}]")
            results.append(("partial-sees-only-its-canvas", bad))

            # ── token mandatory ─────────────────────────────────────────────
            bad = []
            r = await host_call(port, "kernel", {"type": "update_agent", "id": "user", "token_sha256": sha(tok["admin"])})
            if r.get("error"):
                bad.append(f"could not make the token mandatory: {r}")
            r = await host_call(port, "kernel", {"type": "reflect"})
            if "a token is required" not in json.dumps(r):
                bad.append(f"a tokenless call still works: {json.dumps(r)[:200]}")
            fresh = Browser(tmp, "fresh", sc["window"])
            browsers.append(fresh)
            e = await tab(fresh, url, stack=stack)
            if not await e.until("!!document.querySelector('.token-ask')", 15):
                bad.append(f"no token form: {await e.js('document.body.innerText.slice(0,200)')}")
            else:
                await e.js(f"(()=>{{const f=document.querySelector('.token-ask'); f.querySelector('input').value={json.dumps(tok['admin'])}; f.requestSubmit();}})()")
                if not await ready(e):
                    bad.append("the admin token did not open the canvas")
                elif await e.js("document.body.classList.contains('ro')"):
                    bad.append("the admin tab is locked")
            results.append(("token-mandatory", bad))

            for p in (a, b, c, d, e):
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
    tmp = Path(tempfile.mkdtemp(prefix="e2e-users-"))
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
