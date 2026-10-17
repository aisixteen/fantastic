"""Run the canvas scenario (E19) — a REAL headless Chrome over CDP, no model.

The canvas is a thin web UI: the page holds no store and no AI, and its thin
agents are records the host's page loader (`web_loader`, under the host's
`canvas_root` node, rooted at `.fantastic/web`) writes as folders under the
canvas and hands back on boot; a cell's backend is a direct child of
`canvas_root`, beside `static_files`. This runner:

1. builds the product (`fantastic`, whose build packs the canvas app and embeds
   `canvas.zip` — the `canvas_page` agent serves it);
2. runs `fantastic canvas --port N` over a fresh directory (it inits
   from the blueprint and creates the canvas's page, leg, loader and backend);
3. drives the page in headless Chrome (dblclick, typing, drag, close) and
   restarts the host between runs;
4. checks every step on the page AND on disk.

    cd src/lib/python && uv run python -u ../../../e2e/runners/canvas.py canvas

Chrome is required (`CHROME` or the macOS app path); a missing one fails loud.
"""

from __future__ import annotations

import asyncio
import json
import re
import os
import secrets
import shutil
import socket
import subprocess
import sys
import tempfile
import time
import urllib.parse
import urllib.request
from pathlib import Path

E2E = next(d for d in Path(__file__).resolve().parents if (d / ".e2e-root").exists())
sys.path.insert(0, str(E2E))

from e2e_build import build_lock  # noqa: E402
from helpers import oslayer, verdict  # noqa: E402

# The binary's user-home template store (`user_home_store`) reads FANTASTIC_HOME,
# else ~/.config/fantastic. A test must never read or write the owner's real
# home: every binary this runner launches inherits a throwaway one.
_HOME = Path(tempfile.mkdtemp(prefix="fc-e2e-home-"))
oslayer.make_private(_HOME)  # the store refuses a home that is not owner-only
os.environ["FANTASTIC_HOME"] = str(_HOME)

REPO = E2E.parent
SRC = REPO / "src"
# e2e builds go to their OWN target dir: the dev binary (`src/target/debug/fantastic`,
# often linked as `~/.local/bin/fantastic`) keeps the features its owner built it with
E2E_TARGET = SRC / "target" / "e2e"
BIN = oslayer.exe(E2E_TARGET / "debug" / "fantastic")
MAC_CHROME = "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome"
#: The e2e browser is a DESKTOP with a mouse on every OS: a machine with no
#: pointing device (a Linux CI runner) makes Chrome report `(hover: none)`, and
#: the canvas then rightly shows every cell's chrome, as on a phone.
DESKTOP_POINTER = ("--blink-settings=primaryHoverType=2,availableHoverTypes=2,"
                   "primaryPointerType=4,availablePointerTypes=4")
# a terminal's text, from its BUFFER (the WebGL renderer leaves no text in the DOM)
XTERM_TEXT = "((()=>{const t=document.querySelector('.agent-frame .frame-body')?.xterm; if(!t) return ''; const b=t.buffer.active; let s=''; for(let i=0;i<b.length;i++){const l=b.getLine(i); if(l) s+=l.translateToString(true)+'\\n';} return s;})())"



def _scenario_path(scenario_id: str) -> Path:
    for d in sorted((E2E / "tests" / "scenario").iterdir()):
        if d.is_dir() and d.name.split("_", 1)[-1] == scenario_id:
            return d / "scenario.yaml"
    raise SystemExit(f"no scenario {scenario_id!r}")


async def attach(cdp_port: int, timeout: float = 10.0):
    """Chrome's page target, CONNECTED: a fresh Chrome lists its debugging port
    before its page, and can refuse the page's socket (HTTP 404) for a moment
    after listing it — retry both until `timeout`, then fail saying which."""
    import websockets
    deadline = time.time() + timeout
    why = "its debugging port never answered"
    while time.time() < deadline:
        try:
            tabs = json.loads(urllib.request.urlopen(f"http://127.0.0.1:{cdp_port}/json/list").read())
            page_ws = next((t["webSocketDebuggerUrl"] for t in tabs if t.get("type") == "page"), None)
            if page_ws is None:
                why = "it listed no page target"
            else:
                return await websockets.connect(page_ws, max_size=None, ping_interval=None)
        except Exception as e:  # noqa: BLE001 — not up yet: retried until the deadline
            why = f"{type(e).__name__}: {e}"
        await asyncio.sleep(0.2)
    raise SystemExit(f"Chrome on port {cdp_port}: not attached in {timeout:.0f} s — {why}")


def free_port() -> int:
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


# ── build ─────────────────────────────────────────────────────────────────────
def build_bin(into: Path) -> Path:
    with build_lock(E2E_TARGET):  # the build AND the copy: no other build lands in between
        src = Path(os.environ["FANTASTIC_BIN"]).expanduser().resolve() if os.environ.get("FANTASTIC_BIN") else BIN
        if not os.environ.get("FANTASTIC_BIN"):
            r = subprocess.run(["cargo", "build", "-p", "fantastic-tui"], cwd=SRC, capture_output=True, text=True, env={**os.environ, "CARGO_TARGET_DIR": str(E2E_TARGET)})
            if r.returncode != 0:
                raise SystemExit(f"cargo build fantastic-tui failed:\n{r.stderr[-2000:]}")
        b = oslayer.exe(into / "fantastic")
        b.unlink(missing_ok=True)  # never cp over a signed binary (macOS kills it)
        shutil.copy2(src, b)
    return b


# ── the host: the product, headless ───────────────────────────────────────────
class Host:
    def __init__(self, binary: Path, cwd: Path, port: int):
        self.binary, self.cwd, self.port = binary, cwd, port
        self.proc: subprocess.Popen | None = None
        self.log = cwd.parent / "host.log"
        self.url: str | None = None

    def start(self) -> None:
        # The port is named on the first run, read from `web`'s
        # record after that.
        first = not (self.cwd / ".fantastic").exists()
        args = ["canvas", *(["--port", str(self.port)] if first else [])]
        with open(self.log, "a") as f:
            f.write("\n=== start\n")
        f = open(self.log, "a")
        self.proc = subprocess.Popen([str(self.binary), *args], cwd=self.cwd, stdout=f, stderr=subprocess.STDOUT,
                                     **oslayer.group_kwargs())
        deadline = time.time() + 30
        while time.time() < deadline:
            if self.proc.poll() is not None:
                raise SystemExit(f"host exited {self.proc.returncode}:\n{self.log.read_text()[-2000:]}")
            run = self.log.read_text().split("=== start")[-1]
            for line in run.splitlines():
                if line.startswith("fantastic: the canvas: "):
                    self.url = line.split("fantastic: the canvas: ", 1)[1].strip()
                    return
            time.sleep(0.1)
        raise SystemExit(f"the host never announced the canvas:\n{self.log.read_text()[-2000:]}")

    def stop(self) -> int:
        assert self.proc is not None
        oslayer.stop(self.proc)
        try:
            code = self.proc.wait(20)
        except subprocess.TimeoutExpired:
            self.proc.kill()
            raise SystemExit("host did not stop on SIGTERM")
        self.proc = None
        return code


async def host_call(port: int, target: str, payload: dict) -> dict:
    """A host call the way a canvas tab makes one: through `user` — `forward`
    to `target`, or `user`'s own verb when `target` is `user`."""
    import websockets

    body = dict(payload) if target == "user" else {"type": "forward", "target": target, "payload": payload}
    frame = {"type": "call", "target": "user", "payload": body, "id": secrets.token_hex(6)}
    async with websockets.connect(f"ws://127.0.0.1:{port}/user/ws", origin=f"http://127.0.0.1:{port}") as ws:
        await ws.send(json.dumps(frame))
        # only the answer to THIS frame: a connect-time frame (the socket's
        # refused auto-watch of `user`) is not it
        while True:
            m = json.loads(await asyncio.wait_for(ws.recv(), 10))
            if m.get("type") in ("reply", "error") and m.get("id") == frame["id"]:
                return m.get("data", m)


# ── the browser ───────────────────────────────────────────────────────────────
class Page:
    """One CDP page. Every call waits for its own reply; console errors and
    uncaught exceptions are collected for the verdict."""

    def __init__(self, ws):
        self.ws, self.n, self.logs = ws, 0, []

    async def cdp(self, method: str, **params):
        self.n += 1
        me = self.n
        await self.ws.send(json.dumps({"id": me, "method": method, "params": params}))
        while True:
            m = json.loads(await asyncio.wait_for(self.ws.recv(), 30))
            if m.get("method") == "Runtime.exceptionThrown":
                d = m["params"]["exceptionDetails"]
                self.logs.append("EXC " + d.get("exception", {}).get("description", d.get("text", "")))
            elif m.get("method") == "Tracing.dataCollected" and hasattr(self, "trace"):
                self.trace.extend(m["params"]["value"])
            elif m.get("method") == "Tracing.tracingComplete" and hasattr(self, "trace"):
                self.trace_done = True
            elif m.get("method") in ("Network.webSocketFrameReceived", "Network.webSocketFrameSent", "Network.webSocketClosed", "Network.webSocketCreated") and hasattr(self, "ws_log"):
                # a boot trace: every frame's head, each way (CANVAS_WS_LOG)
                p = m["params"]
                pd = (p.get("response") or {}).get("payloadData", "")
                self.ws_log.append(f"{time.time():.3f} {m['method'].split('.')[-1]} {len(pd)} {pd[:160]}")
            elif m.get("method") in ("Network.webSocketFrameReceived", "Network.webSocketFrameSent") and hasattr(self, "ws_frames"):
                # a load probe counts the page's WS traffic (frames, payload bytes)
                way = self.ws_frames["in" if m["method"].endswith("Received") else "out"]
                way[0] += 1
                way[1] += len(m["params"]["response"].get("payloadData", ""))
            elif m.get("method") == "Runtime.consoleAPICalled" and m["params"]["type"] == "error":
                self.logs.append("ERR " + " ".join(str(a.get("value", a.get("description", ""))) for a in m["params"]["args"]))
            if m.get("id") == me:
                if "error" in m:
                    raise RuntimeError(f"{method}: {m['error']}")
                return m.get("result", {})

    async def js(self, expr: str):
        r = await self.cdp("Runtime.evaluate", expression=expr, returnByValue=True, awaitPromise=True)
        return r.get("result", {}).get("value")

    async def until(self, expr: str, timeout: float = 10.0) -> bool:
        deadline = time.time() + timeout
        while time.time() < deadline:
            if await self.js(expr):
                return True
            await asyncio.sleep(0.1)
        return False

    async def open(self, url: str) -> None:
        """The page AT `url`, loaded. A navigation the host refused (its server a
        moment from listening) leaves Chrome's own error page — complete, with a
        body — so the check is the page's origin, and a miss navigates again."""
        origin = json.dumps(urllib.parse.urlsplit(url)._replace(path="", query="", fragment="").geturl())
        loaded = f"document.readyState === 'complete' && location.origin === {origin} && !!document.querySelector('#status, .status, body')"
        for _ in range(10):
            nav = await self.cdp("Page.navigate", url=url)
            if not nav.get("errorText") and await self.until(loaded, 15):
                return
            await asyncio.sleep(1.0)
        raise SystemExit(f"the page never loaded {url}: {nav.get('errorText') or 'not at its origin'}")

    async def viewport(self, width: int, height: int) -> None:
        """The page's viewport AT LEAST `width` x `height` — what the window size
        gives on macOS and Linux (where a headless window is never narrower than
        ~500px, which the phone-wide probes are drawn for). Windows takes its
        frame out of the window (1400x900 drew 1354x775) and a probe's points near
        the bottom fell off the page: there, and only there, it is set."""
        have = await self.js("[innerWidth, innerHeight]") or [0, 0]
        w, h = max(width, have[0]), max(height, have[1])
        if [w, h] != have:
            await self.cdp("Emulation.setDeviceMetricsOverride", width=w, height=h, deviceScaleFactor=0, mobile=False)

    async def mouse(self, typ: str, x: float, y: float, clicks: int = 1) -> None:
        await self.cdp("Input.dispatchMouseEvent", type=typ, x=x, y=y, button="left", clickCount=clicks)

    async def type(self, text: str) -> None:
        await self.js("document.querySelector('.xterm-helper-textarea')?.focus()")
        await self.cdp("Input.insertText", text=text)
        await self.cdp("Input.dispatchKeyEvent", type="keyDown", key="Enter", code="Enter",
                       windowsVirtualKeyCode=13, text="\r")
        await self.cdp("Input.dispatchKeyEvent", type="keyUp", key="Enter", code="Enter", windowsVirtualKeyCode=13)

    async def rect(self, sel: str) -> dict | None:
        return await self.js(f"(()=>{{const e=document.querySelector({json.dumps(sel)}); if(!e) return null;"
                             "const r=e.getBoundingClientRect(); return {x:r.x,y:r.y,w:r.width,h:r.height};})()")


# ── disk ──────────────────────────────────────────────────────────────────────
async def prompt(page: Page, bad: list[str]) -> None:
    """The shell is up: its prompt is on the screen (a login shell started a
    moment ago — after a restart's ▶ — can take seconds; keys typed before it
    reads them are its to lose)."""
    if not await page.until(XTERM_TEXT + ".trim().length > 0", 20):
        bad.append("the shell drew no prompt")
    await asyncio.sleep(0.3)


def yaml_of(p: Path) -> dict:
    import yaml

    return yaml.safe_load(oslayer.read_text(p)) or {}


# the root view's records, under the page root `canvas`
ROOT_VIEW = ".fantastic/web/agents/canvas/agents/view_root/agents"


def members(cwd: Path) -> list[dict]:
    """The root view's cells — every child but its singleton background and a
    double click's ephemeral selector (the popup's agent, never a cell)."""
    d = cwd / ROOT_VIEW
    every = []
    for f in sorted(d.glob("*/agent.yaml")) if d.is_dir() else []:
        # the loader may remove a folder between the listing and the read: a
        # record gone mid-check is simply not there
        try:
            every.append(dict(yaml_of(f), _dir=f.parent.name))
        except FileNotFoundError:
            pass
    return [m for m in every if m.get("handler_module") not in ("background.ts", "selector.ts", "layout.ts")]


def rect_of(cwd: Path, cell: str) -> dict:
    """Where `cell` sits in the canvas theme: the root view's `layout_canvas`
    agent's rects (a cell carries no geometry)."""
    f = cwd / ROOT_VIEW / "layout_canvas" / "agent.yaml"
    try:
        return dict(((yaml_of(f) or {}).get("rects") or {}).get(cell) or {})
    except FileNotFoundError:
        return {}


# the cells' backends on the host: children of `canvas_root`
FOLDER = ".fantastic/agents/user/agents/web/agents/canvas_page/agents/canvas_root"


def group_ids(cwd: Path) -> list[str]:
    """The cells' backends: `canvas_root`'s children that carry a `cell_id`."""
    d = cwd / FOLDER / "agents"
    if not d.is_dir():
        return []
    return sorted(r["id"] for f in d.glob("*/agent.yaml") if "cell_id" in (r := yaml_of(f)))


def host_record(cwd: Path, agent_id: str) -> Path | None:
    for f in (cwd / ".fantastic").rglob("agent.yaml"):
        if ".fantastic/web" in str(f):
            continue
        if yaml_of(f).get("id") == agent_id:
            return f
    return None


def wait_disk(cond, timeout: float = 15.0) -> bool:
    deadline = time.time() + timeout
    while time.time() < deadline:
        if cond():
            return True
        time.sleep(0.1)
    return cond()


# ── one run ───────────────────────────────────────────────────────────────────
async def restart(spec, page: Page, host: Host, cwd: Path, url: str, state: dict, exp: dict, bad: list[str]) -> None:
    """Stop the host, apply an OFFLINE edit to the disk (a crash's leftovers),
    start it again and reload the page."""
    # ONE KERNEL PER DIRECTORY: while it runs, ANY argument is refused, exit 2,
    # "already running" — even a --port that would change it (a swallowed
    # command confuses more than a refusal).
    if isinstance(spec, dict) and spec.get("wrong_port"):
        r = subprocess.run([str(host.binary), "canvas", "--port", str(host.port + 1)],
                           cwd=cwd, capture_output=True, text=True, timeout=60)
        said = r.stdout + r.stderr
        if r.returncode != 2 or "already running" not in said:
            bad.append(f"an argument against the running kernel was not refused: exit {r.returncode}: {said[-300:]!r}")
    code = host.stop()
    if code != 0:
        bad.append(f"host exited {code} on SIGTERM")
    if exp.get("lock_released_between") and (cwd / ".fantastic/lock.yaml").exists():
        bad.append("lock.yaml left behind by the stop")
    offline = spec.get("offline") if isinstance(spec, dict) else None
    ms = members(cwd)
    if offline == "remove_backend":
        bid = ms[0].get("backend_id") if ms else None
        rec = host_record(cwd, bid) if bid else None
        if rec is None:
            bad.append(f"no backend record to remove ({bid})")
        else:
            shutil.rmtree(rec.parent)
    elif offline == "remove_member":
        if not ms:
            bad.append("no member folder to remove")
        else:
            state["orphan"] = ms[0].get("backend_id")
            shutil.rmtree(cwd / ROOT_VIEW / ms[0]["_dir"])
    elif offline is not None:
        raise SystemExit(f"unknown offline edit {offline!r}")
    host.start()
    tree = await host_call(host.port, "web_loader", {"type": "load_tree"})
    state["tree"] = [(r.get("id"), r.get("parent_id")) for r in tree.get("records", [])]
    # a view's layouts (where its cells sit, per theme) are no members
    state["tree_layouts"] = [r.get("id") for r in tree.get("records", []) if r.get("handler_module") == "layout.ts"]
    await page.open(url)


async def click(page: Page, sel: str, bad: list[str]) -> None:
    # a target drawn a moment after the page (a stopped terminal's ▶, past its attach)
    await page.until(f"!!document.querySelector({json.dumps(sel)})", 10)
    r = await page.rect(sel)
    if r is None:
        bad.append(f"nothing to click at {sel}")
        return
    await page.mouse("mousePressed", r["x"] + r["w"] / 2, r["y"] + r["h"] / 2)
    await page.mouse("mouseReleased", r["x"] + r["w"] / 2, r["y"] + r["h"] / 2)


async def step(run: dict, page: Page, host: Host, cwd: Path, url: str, state: dict) -> list[str]:
    bad: list[str] = []
    exp = run.get("expect") or {}
    if run.get("restart"):
        await restart(run["restart"], page, host, cwd, url, state, exp, bad)
    if "dblclick" in run:
        before = await page.js("document.querySelectorAll('.xterm-helper-textarea').length")
        x, y = run["dblclick"]["x"], run["dblclick"]["y"]
        for c in (1, 2):
            await page.mouse("mousePressed", x, y, c)
            await page.mouse("mouseReleased", x, y, c)
        # the double click opens the SPOT INPUT: pick `terminal` in it
        if not await page.until("!!document.querySelector('.action-popup input')", 10):
            bad.append("no action popup after the double click")
        await page.cdp("Input.insertText", text="terminal")
        await page.until("[...document.querySelectorAll('.action-popup .tile')].length === 1", 5)
        await page.cdp("Input.dispatchKeyEvent", type="keyDown", key="Enter", code="Enter", windowsVirtualKeyCode=13)
        await page.cdp("Input.dispatchKeyEvent", type="keyUp", key="Enter", code="Enter", windowsVirtualKeyCode=13)
        if not await page.until(f"document.querySelectorAll('.xterm-helper-textarea').length > {before}", 10):
            bad.append("no terminal appeared after the double click")
        wait_disk(lambda: len(members(cwd)) > 0)
    if "drag" in run:
        r = await page.rect(".agent-frame .frame-head")
        if r is None:
            bad.append("no frame head to drag")
        else:
            x0, y0 = r["x"] + r["w"] / 2 - 40, r["y"] + r["h"] / 2
            # the head shows while the pointer is on the cell: point at the
            # content first, then up onto the head, as a hand does
            body = await page.rect(".agent-frame .frame-body")
            if body is not None:
                await page.cdp("Input.dispatchMouseEvent", type="mouseMoved", x=body["x"] + body["w"] / 2, y=body["y"] + 20)
                await page.cdp("Input.dispatchMouseEvent", type="mouseMoved", x=x0, y=y0)
            await page.mouse("mousePressed", x0, y0)
            for i in range(1, 9):
                await page.cdp("Input.dispatchMouseEvent", type="mouseMoved", x=x0 + run["drag"]["dx"] * i / 8,
                               y=y0 + run["drag"]["dy"] * i / 8, button="left")
            await page.mouse("mouseReleased", x0 + run["drag"]["dx"], y0 + run["drag"]["dy"])
    if run.get("close"):
        await click(page, ".agent-frame .close", bad)
    if "click" in run:
        state["prev_backend"] = (members(cwd) or [{}])[0].get("backend_id")
        await click(page, run["click"], bad)
    if "type" in run:
        if not await page.until("!!document.querySelector('.xterm-helper-textarea')", 10):
            bad.append("no terminal to type into")
        await prompt(page, bad)
        await page.type(oslayer.for_os(run["type"]))
    if "before_reload_xterm" in run:
        w = run["before_reload_xterm"]
        # a login shell under load can take a while to answer; this returns on the hit
        if not await page.until(XTERM_TEXT + f".includes({json.dumps(w)})", 30):
            bad.append(f"the shell never answered {w!r} before the reload")
    if run.get("reload_page"):
        await page.open(url)
    if "type_after" in run:
        if not await page.until("!!document.querySelector('.xterm-helper-textarea')", 10):
            bad.append("no terminal to type into after the reload")
        await prompt(page, bad)
        await page.type(oslayer.for_os(run["type_after"]))
    if "then_restart" in run:
        await restart(run["then_restart"], page, host, cwd, url, state, exp, bad)

    # ── checks ──
    for word in exp.get("page_contains", []):
        if not await page.until(f"document.body.innerText.includes({json.dumps(word)})", 10):
            bad.append(f"page lacks {word!r}")
    for word in exp.get("xterm_contains", []):
        if not await page.until(XTERM_TEXT + f".includes({json.dumps(word)})", 10):
            txt = await page.js(XTERM_TEXT)
            bad.append(f"xterm lacks {word!r}: {txt[-200:]!r}")
    for p in exp.get("files_exist", []):
        if not wait_disk(lambda p=p: (cwd / p).exists()):
            bad.append(f"missing {p}")
    for p in exp.get("files_absent", []):
        if (cwd / p).exists():
            bad.append(f"present {p} (must not exist)")
    if "canvas_members" in exp:
        n = exp["canvas_members"]
        if not wait_disk(lambda: len(members(cwd)) == n):
            bad.append(f"{len(members(cwd))} members under the root view, want {n}: {[m['_dir'] for m in members(cwd)]}")
    if "member" in exp:
        want = exp["member"]
        wait_disk(lambda: bool(members(cwd)) and all(members(cwd)[0].get(k) == v for k, v in want.items()))
    ms = members(cwd)
    m = ms[0] if ms else None
    if "member" in exp and m is not None:
        for k, v in exp["member"].items():
            if m.get(k) != v:
                bad.append(f"member {k} {m.get(k)!r} != {v!r}")
        if m.get("id") != m["_dir"]:
            bad.append(f"member folder {m['_dir']} holds id {m.get('id')}")
        state["member"] = m
        state["rect"] = rect_of(cwd, m.get("id", ""))
    if exp.get("not_top_level") and m is not None:
        top = cwd / ".fantastic/web/agents" / m["id"]
        if top.exists():
            bad.append(f"{m['id']} also written at the loader's top level")
    if exp.get("backend_in_group") and m is not None:
        bid = m.get("backend_id")
        if not wait_disk(lambda: bid in group_ids(cwd)):
            where = host_record(cwd, bid)
            bad.append(f"backend {bid} is not a child of the root view's folder (record at {where})")
    if exp.get("backend_kept") and m is not None and m.get("backend_id") != (state.get("member") or {}).get("backend_id"):
        bad.append("the page reload changed the backend")
    if exp.get("view_bound") and m is not None:
        bid = m.get("backend_id")

        def bound() -> dict:
            # a record being rewritten (a grant update) reads empty for an instant
            rec = host_record(cwd, bid) if bid else None
            try:
                return yaml_of(rec) if rec is not None else {}
            except Exception:  # noqa: BLE001
                return {}
        wait_disk(lambda: bound().get("cell_id") == m.get("id"))
        b = bound()
        if b.get("cell_id") != m.get("id"):
            bad.append(f"backend {bid} carries cell_id={b.get('cell_id')!r}, want {m.get('id')!r}")
        # the cell's own grant names its backend, and nothing else (the
        # kernel's own `$self` aside)
        cc = {k: v for k, v in (m.get("can_call") or {}).items() if k != "$self"}
        if list(cc) != [bid] or "write" not in (cc.get(bid) or []):
            bad.append(f"cell can_call {cc!r} does not grant exactly its backend {bid}")
    if exp.get("backend_changed") and m is not None and m.get("backend_id") == state.get("prev_backend"):
        bad.append(f"the respawn kept backend_id {m.get('backend_id')}")
    if "group_size" in exp:
        n = exp["group_size"]
        if not wait_disk(lambda: len(group_ids(cwd)) == n):
            bad.append(f"{len(group_ids(cwd))} backends in the root view's folder, want {n}: {group_ids(cwd)}")
    if "backend_record" in exp:
        bid = (state.get("member") or {}).get("backend_id")
        if not bid:
            bad.append("no backend_id on the member")
        elif wait_disk(lambda: (host_record(cwd, bid) is not None) == exp["backend_record"]) is False:
            bad.append(f"host record of {bid}: {'missing' if exp['backend_record'] else 'still there'}")
    if exp.get("moved") and m is not None:
        cell = m.get("id", "")
        before = state.get("rect") or {}
        if not wait_disk(lambda: rect_of(cwd, cell).get("x") != before.get("x")):
            bad.append(f"x did not move: {before.get('x')} → {rect_of(cwd, cell).get('x')}")
        r = rect_of(cwd, cell)
        state["moved"] = (r.get("x"), r.get("y"))
    if exp.get("kept_position") and m is not None:
        r = rect_of(cwd, m.get("id", ""))
        if (r.get("x"), r.get("y")) != state.get("moved"):
            bad.append(f"position after restart {(r.get('x'), r.get('y'))} != {state.get('moved')}")
    # `canvas` is headless: the kernel's message stream is the host's stdout, one line
    # per row (`[<id>] <agent> <kind|role> <verb>? · <first line>`), no escapes.
    if exp.get("stream"):
        run = host.log.read_text().split("=== start")[-1]
        lines = [l for l in run.splitlines() if re.match(r"^\[\d+\] \S+ \S+", l)]
        if not lines:
            bad.append("the headless stream printed no `[<id>] <agent> <kind>` line")
        if "\x1b[" in run:
            bad.append("the headless stream carries escape codes")
    if "load_tree" in exp:
        tree = dict(state.get("tree", []))
        want = exp["load_tree"]
        if "canvas" in want and ("canvas" not in tree or tree.get("view_root") != "canvas"):
            bad.append(f"load_tree has no canvas with its root view: {state.get('tree')}")
        if "canvas" in want and tree.get("background") != "view_root":
            bad.append(f"load_tree has no background singleton under the root view: {state.get('tree')}")
        others = [i for i in tree if i not in ("canvas", "view_root", "web_loader", "background") and i not in state.get("tree_layouts", [])]
        if "member_parent" in want:
            if len(others) != 1 or tree[others[0]] != want["member_parent"]:
                bad.append(f"load_tree members {[(i, tree[i]) for i in others]}, want one under {want['member_parent']}")
        elif others:
            bad.append(f"load_tree still has {others}")
    if "page_frames" in exp:
        await asyncio.sleep(1.0)
        n = await page.js("document.querySelectorAll('.agent-frame').length")
        if n != exp["page_frames"]:
            bad.append(f"{n} frames on the page, want {exp['page_frames']}")
    for line in page.logs:
        bad.append(f"page: {line[:240]}")
    page.logs.clear()
    return bad


async def run_all(sc: dict, tmp: Path) -> bool:
    import websockets

    cwd = tmp / "work"
    cwd.mkdir(parents=True)
    binary = build_bin(tmp)
    port = free_port()
    chrome = os.environ.get("CHROME") or MAC_CHROME
    if not Path(chrome).exists():
        raise SystemExit(f"no Chrome at {chrome}: set CHROME")
    cdp_port = free_port()
    w = sc["window"]
    br = subprocess.Popen([chrome, "--headless=new", "--use-angle=swiftshader", "--enable-unsafe-swiftshader", DESKTOP_POINTER, f"--window-size={w['width']},{w['height']}",
                           f"--remote-debugging-port={cdp_port}", f"--user-data-dir={tmp / 'chrome'}",
                           "--no-first-run", "about:blank"], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    host = Host(binary, cwd, port)
    failures = 0
    try:
        host.start()
        url = host.url
        async with await attach(cdp_port, 15) as ws:
            page = Page(ws)
            await page.cdp("Runtime.enable")
            await page.cdp("Page.enable")
            await page.viewport(w["width"], w["height"])
            await page.open(url)
            state: dict = {}
            for run in sc["runs"]:
                bad = await step(run, page, host, cwd, url, state)
                print(f"  {'✓' if not bad else '✗'} {run['name']}", flush=True)
                for b in bad:
                    print(f"      {b}", flush=True)
                failures += bool(bad)
    finally:
        if host.proc is not None:
            host.stop()
        br.terminate()
        try:
            br.wait(10)
        except subprocess.TimeoutExpired:
            br.kill()
    return failures == 0


def main(argv: list[str]) -> int:
    import yaml

    if not argv or argv[0].startswith("-"):
        raise SystemExit("usage: canvas.py <scenario-id>")
    sc = yaml.safe_load(_scenario_path(argv[0]).read_text())
    print(f"E19_{sc['id']} · {sc['title']}", flush=True)
    tmp = Path(tempfile.mkdtemp(prefix="e2e-canvas-"))
    try:
        ok = asyncio.run(run_all(sc, tmp))
    finally:
        if not os.environ.get("E2E_KEEP"):
            shutil.rmtree(tmp, ignore_errors=True)
        else:
            print(f"  kept {tmp}")
    verdict(ok, "all runs green" if ok else "failures above")
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
