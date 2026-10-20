"""THE HOME TEST FIXTURE — `HomeRig`, the canvas rig's world (`canvas_live.Rig`:
its artifacts dir, its private copy of `fantastic`, its throwaway home) with the
HOME in it: the test-only host `fantastic-home-host` (the home kernel,
in-process — the desktop app is its only real host), the `fantastic canvas`
processes it lists, and headless Chrome tabs on the Home page.

Every process of a run shares ONE home dir, `<art>/home`, so every fantastic
writes the registry the home reads. The home kernel creates that dir itself
(`ensure_private_dir`), as `e2e/runners/home.py` (E24) does: its strict check
then judges a dir the kernel made, never one the harness did (a dir the harness
made private with `icacls` was read as "readable by others" on the Windows CI
runner). So `start_home()` runs before any fantastic does.

    rig = HomeRig(__file__); rig.build(); rig.start_home()
    await rig.fantastic("alpha")                 # running, registered
    await rig.fantastic("beta", running=False)   # registered, stopped cleanly
    async with rig.page() as pg: …               # Chrome on the Home
    rig.stop_all()
"""

from __future__ import annotations

import asyncio
import json
import os
import re
import subprocess
import time
from contextlib import asynccontextmanager
from pathlib import Path

import yaml

from _lib.canvas_live import DESKTOP_POINTER, MAC_CHROME, Page, Rig, attach, free_port, oslayer, verdict  # noqa: F401 — `verdict` for the probes

# `e2e/runners` is on the path once `canvas_live` is imported
from canvas import build_home_host, host_call  # noqa: E402, I001 — `host_call` for the probes

ANNOUNCE = "fantastic-home-host: the home: "
CANVAS_ANNOUNCE = re.compile(r"^fantastic: the canvas: (\S+)", re.MULTILINE)


def canonical(p: Path) -> str:
    """A folder as a kernel running there names it: links resolved on POSIX; on
    Windows absolute, as its `current_dir()` reports it."""
    return os.path.abspath(p) if oslayer.WINDOWS else os.path.realpath(p)


def yaml_of(p: Path) -> dict:
    return yaml.safe_load(oslayer.read_text(p)) or {}


def lock_pid(folder: Path) -> int | None:
    """The live pid in `folder`'s lock file (a clean stop removes the file)."""
    try:
        pid = yaml_of(folder / ".fantastic" / "lock.yaml").get("pid")
    except OSError:
        return None
    return pid if isinstance(pid, int) and oslayer.pid_alive(pid) else None


def os_shell() -> tuple[str, str]:
    """`(os, webview)` of this machine, as a native shell's flag names them."""
    if oslayer.WINDOWS:
        return "windows", "webview2"
    if os.uname().sysname == "Darwin":
        return "macos", "wkwebview"
    return "linux", "webkitgtk"


IS_MAC = not oslayer.WINDOWS and os.uname().sysname == "Darwin"
#: CDP `Input.dispatchKeyEvent` modifiers for the Home's mod key: Meta (4) on a Mac, Ctrl (2) elsewhere.
MOD = 4 if IS_MAC else 2


class HomeRig(Rig):
    """One test's world, with the home host in it."""

    def __init__(self, test_file: str):
        super().__init__(test_file, live=False)
        self.home_port = free_port()
        self.home_log = self.art / "home-host.log"
        self.home_proc: subprocess.Popen | None = None
        self.home_url: str | None = None
        #: name → its folder, and the processes this rig started itself
        self.folders: dict[str, Path] = {}
        self.procs: list[subprocess.Popen] = []
        self.tabs = 0
        self.click_logs: list[list] = []  # each tab's missed clicks, kept past the tab

    # ── the binaries ────────────────────────────────────────────────────────
    def build(self, *features: str) -> None:
        super().build(*features)
        self.host_bin = build_home_host(self.art)

    # ── the home host ───────────────────────────────────────────────────────
    def start_home(self) -> None:
        with open(self.home_log, "a") as f:
            f.write("\n=== start\n")
        self.home_proc = subprocess.Popen(
            [str(self.host_bin), "--fantastic", str(self.binary), "--port", str(self.home_port)],
            env=self.env(), stdout=open(self.home_log, "a"), stderr=subprocess.STDOUT, stdin=subprocess.DEVNULL,
            **oslayer.group_kwargs(),
        )
        deadline = time.time() + 60
        while time.time() < deadline:
            if self.home_proc.poll() is not None:
                access = "\n".join(oslayer.describe_access(p) for p in (self.art, self.home) if p.exists())
                raise SystemExit(f"the home host exited {self.home_proc.returncode}:\n{self.home_log.read_text()[-2000:]}\n── access:\n{access}")
            for line in self.home_log.read_text().split("=== start")[-1].splitlines():
                if line.startswith(ANNOUNCE):
                    self.home_url = line[len(ANNOUNCE):].strip()
                    return
            time.sleep(0.1)
        raise SystemExit(f"the home host never announced its url:\n{self.home_log.read_text()[-2000:]}")

    def stop_home(self) -> None:
        if self.home_proc is None:
            return
        oslayer.stop(self.home_proc)
        try:
            code = self.home_proc.wait(30)
        except subprocess.TimeoutExpired:
            oslayer.kill_group(self.home_proc.pid)
            self.home_proc = None
            raise SystemExit("the home host did not stop when asked")
        self.home_proc = None
        if code != 0:
            raise SystemExit(f"the home host exited {code}:\n{self.home_log.read_text()[-2000:]}")

    def restart_home(self) -> None:
        """Stop the host and boot it again, on the same port."""
        self.stop_home()
        self.start_home()

    async def home_call(self, payload: dict) -> dict:
        """A `home` call the way the page makes one (`user.forward`)."""
        return await host_call(self.home_port, "home", payload)

    async def until_state(self, what: str, ok, timeout: float) -> dict:
        """The home's state once `ok(state)` holds — or a SystemExit naming the last one."""
        deadline = time.time() + timeout
        state: dict = {}
        while time.time() < deadline:
            state = await self.home_call({"type": "list"})
            if state.get("type") == "state" and ok(state):
                return state
            await asyncio.sleep(0.25)
        raise SystemExit(f"{what}: not within {timeout:.0f} s — last state {json.dumps(state)[:1500]}")

    # ── fantastics ──────────────────────────────────────────────────────────
    async def fantastic(self, name: str, running: bool = True) -> Path:
        """`<art>/<name>` running `fantastic canvas`, its home switch on (so it
        registers); with `running=False`, then stopped cleanly."""
        folder = self.art / name
        folder.mkdir(exist_ok=True)
        self.folders[name] = folder
        log = self.art / f"{name}.log"
        port = free_port()
        with open(log, "a") as f:
            f.write("\n=== start\n")
        proc = subprocess.Popen([str(self.binary), "canvas", "--port", str(port)], cwd=folder, env=self.env(), stdout=open(log, "a"),
                                stderr=subprocess.STDOUT, stdin=subprocess.DEVNULL, **oslayer.group_kwargs())
        self.procs.append(proc)
        deadline = time.time() + 60
        while not CANVAS_ANNOUNCE.search(log.read_text().split("=== start")[-1]):
            if proc.poll() is not None or time.time() > deadline:
                raise SystemExit(f"fantastic canvas in {folder} never announced its canvas ({proc.poll()}):\n{log.read_text()[-2000:]}")
            await asyncio.sleep(0.1)
        r = await host_call(port, "user_store", {"type": "register_home_folder", "value": True})
        if r.get("error"):
            raise SystemExit(f"{name}'s home switch: {r}")
        if not running:
            oslayer.stop(proc)
            try:
                proc.wait(30)
            except subprocess.TimeoutExpired:
                raise SystemExit(f"{name} did not stop when asked")
            deadline = time.time() + 20
            while (folder / ".fantastic" / "lock.yaml").exists():
                if time.time() > deadline:
                    raise SystemExit(f"{name}'s lock was never freed")
                await asyncio.sleep(0.1)
        return folder

    # ── the disk ────────────────────────────────────────────────────────────
    def registry_files(self) -> list[Path]:
        return sorted((self.home / "fantastics").glob("*/fantastic.yaml"))

    def registry(self, guid: str) -> dict | None:
        """The registry entry of `guid` (`<home>/fantastics/*/fantastic.yaml`)."""
        for f in self.registry_files():
            try:
                doc = yaml_of(f)
            except OSError:
                continue  # rewritten between the listing and the read
            if doc.get("guid") == guid or f.parent.name == guid:
                return doc
        return None

    def registry_order(self) -> list[str]:
        """The registry's guids in the home's order: `(stored_at, folder)`."""
        rows = [(yaml_of(f).get("stored_at") or 0, f.parent.name, yaml_of(f).get("guid") or f.parent.name) for f in self.registry_files()]
        return [g for _, _, g in sorted(rows)]

    # ── the browser ─────────────────────────────────────────────────────────
    @asynccontextmanager
    async def page(self, width: int = 1400, height: int = 900, before_load: str | None = None):
        """A Chrome tab on the Home, once `#home` is drawn; `before_load` runs in
        every document before its scripts (`Page.addScriptToEvaluateOnNewDocument`)."""
        self.tabs += 1
        tab = self.tabs
        cdp = free_port()
        mode = [] if os.environ.get("CANVAS_HEADFUL") == "1" else ["--headless=new", "--use-angle=swiftshader", "--enable-unsafe-swiftshader"]
        chrome = subprocess.Popen([os.environ.get("CHROME") or MAC_CHROME, *mode, DESKTOP_POINTER, f"--window-size={width},{height}",
                                   f"--remote-debugging-port={cdp}", f"--user-data-dir={self.art / f'chrome-{tab}'}", "--no-first-run", "about:blank"],
                                  stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        try:
            async with await attach(cdp) as ws:
                pg = Page(ws)
                self.misses = pg.misses
                await pg.cdp("Runtime.enable")
                await pg.cdp("Page.enable")
                # the page behaves as the focused window's, as it is for a user: a
                # CI runner's Chrome window is not the OS's key window (macOS),
                # and an unfocused page takes no key events
                await pg.cdp("Emulation.setFocusEmulationEnabled", enabled=True)
                await pg.viewport(width, height)
                # every keydown the Home's window sees, for `key`'s log
                await pg.cdp("Page.addScriptToEvaluateOnNewDocument", source=KEY_RECORDER)
                pg.key_log, pg.click_log = [], []
                self.click_logs.append(pg.click_log)
                if before_load is not None:
                    await pg.cdp("Page.addScriptToEvaluateOnNewDocument", source=before_load)
                await self.open_home(pg)
                yield pg
                html = await pg.js("document.documentElement.outerHTML")
                (self.art / f"page-{tab}.html").write_text(html or "")
                (self.art / f"page_errors-{tab}.json").write_text(json.dumps(pg.logs, indent=1))
        finally:
            chrome.terminate()
            try:
                chrome.wait(10)
            except subprocess.TimeoutExpired:
                chrome.kill()

    async def open_home(self, pg: Page) -> None:
        """Navigate `pg` to the Home (again) and wait until it is drawn."""
        await pg.open(self.home_url)
        if not await pg.until("!!document.getElementById('home')", 30):
            text = await pg.js("document.body.innerText")
            raise SystemExit(f"the Home never drew #home: {text!r} · {pg.logs[:3]}")

    # ── the end ─────────────────────────────────────────────────────────────
    def stop_all(self) -> None:
        """Every fantastic stopped (its lock holder, the way the home stops one),
        then the host."""
        pids = [p for p in (lock_pid(f) for f in self.folders.values()) if p is not None]
        for pid in pids:
            oslayer.stop_fantastic(self.binary, pid)

        def gone(pid: int) -> bool:
            # One this rig started is ITS child: until it is reaped (`poll`) it is
            # a zombie, which `pid_alive` reports alive and macOS's `killpg`
            # refuses (EPERM). The home reaps the ones it started itself.
            mine = next((p for p in self.procs if p.pid == pid), None)
            return mine.poll() is not None if mine is not None else not oslayer.pid_alive(pid)

        deadline = time.time() + 20
        while time.time() < deadline and not all(gone(p) for p in pids):
            time.sleep(0.2)
        for pid in pids:
            if not gone(pid):
                oslayer.kill_group(pid)
        for proc in self.procs:
            try:
                proc.wait(5)
            except subprocess.TimeoutExpired:
                oslayer.kill_group(proc.pid)
        try:
            self.stop_home()
        finally:
            if self.home_proc is not None:
                oslayer.kill_group(self.home_proc.pid)

    def finish(self, ok: bool, note: str) -> tuple[bool, str]:
        if not ok and self.home_log.exists():
            print("home-host.log (tail):\n" + "\n".join(self.home_log.read_text(errors="replace").splitlines()[-40:]))
        return super().finish(ok, note)


# ── the state stream ─────────────────────────────────────────────────────────
class StateLog:
    """Every `state` the home emits, as a page sees it on `/home/ws`: per state
    its `seq`, `pages`, `selection`, and each row's `[status, canvas url]`.
    Every `diagnostics` the home emits (a running row's demotion, with the
    probe that caused it) lands in `log.demotions`.

        async with StateLog(rig.home_port) as log: …
        log.states, log.demotions"""

    def __init__(self, port: int):
        self.port, self.states, self.demotions, self.task, self.ws = port, [], [], None, None

    async def __aenter__(self) -> StateLog:
        import websockets

        self.ws = await websockets.connect(f"ws://127.0.0.1:{self.port}/home/ws")

        async def read() -> None:
            try:
                async for m in self.ws:
                    frame = json.loads(m)
                    p = frame.get("payload") or {}
                    if frame.get("type") == "event" and p.get("type") == "state":
                        self.states.append({"seq": p.get("seq"), "pages": p.get("pages"), "selection": p.get("selection"),
                                            "rows": {r["id"]: [r["status"], (r.get("canvas") or {}).get("url")] for r in p.get("rows", [])}})
                    elif frame.get("type") == "event" and p.get("type") == "diagnostics":
                        self.demotions.append({"seq": p.get("seq"), "demotions": p.get("demotions")})
            except Exception:  # noqa: BLE001 — the socket closing ends the log
                pass

        self.task = asyncio.create_task(read())
        return self

    async def __aexit__(self, *exc) -> None:
        await self.ws.close()
        await self.task


# ── page helpers ─────────────────────────────────────────────────────────────
def q(sel: str) -> str:
    """`document.querySelector(sel)` as a JS expression."""
    return f"document.querySelector({json.dumps(sel)})"


#: Records every keydown the top window sees (`window.__keys`).
KEY_RECORDER = ("if (window.top === window) { window.__keys = []; window.addEventListener('keydown', (e) =>"
                " window.__keys.push(e.key + (e.metaKey ? '+meta' : '') + (e.ctrlKey ? '+ctrl' : '')), true); }")

#: Give the Home's document the focus: a canvas frame that holds it keeps the
#: keys (shortcuts inside a frame are out of scope, #47). What was focused, and
#: whether the document has the focus now.
FOCUS_HOME = ("(()=>{const a=document.activeElement; const was=a?a.tagName+(a.className?'.'+a.className:''):null;"
              " if(a&&a.tagName==='IFRAME') a.blur(); window.focus();"
              " return {was, has_focus: document.hasFocus(), active: document.activeElement?.tagName ?? null,"
              " seen: (window.__keys||[]).length};})()")


async def click(pg: Page, sel: str, timeout: float = 10) -> bool:
    """A real mouse click at the middle of `sel`, once it is shown, STILL (the
    carousel slides a page in, and a click mid-slide lands beside it) and the
    top element at that point is `sel` itself (nothing covers it). A miss is
    logged with EVERY sample taken (rect, what the point hit, shown), not
    just the last (`pg.click_log`)."""
    probe = (f"(()=>{{const e={q(sel)}; if(!e) return null; const r=e.getBoundingClientRect();"
             " const shown=r.width>0&&r.height>0&&r.left>=0&&r.right<=innerWidth;"
             " const h=shown?document.elementFromPoint(r.x+r.width/2, r.y+r.height/2):null;"
             " return {x:r.x, y:r.y, w:r.width, h:r.height, shown, hits: !!h&&(h===e||e.contains(h)),"
             " hit: h ? h.tagName+(h.className&&typeof h.className==='string'?'.'+h.className.split(' ').join('.'):'') : null};})()")
    deadline = asyncio.get_running_loop().time() + timeout
    r, last, samples = None, None, []
    while asyncio.get_running_loop().time() < deadline:
        r = await pg.js(probe)
        samples.append(r)
        if r is not None and r["shown"] and r["hits"] and last is not None and \
                (r["x"], r["y"], r["w"], r["h"]) == (last["x"], last["y"], last["w"], last["h"]):
            break
        last = r
        await asyncio.sleep(0.1)
    else:
        miss = {"click": sel, "last": r, "samples": samples}
        getattr(pg, "click_log", []).append(miss)
        pg.misses.append(f"click {sel}: not shown, still and on top within {timeout:g}s — {json.dumps(r)}")
        return False
    x, y = r["x"] + r["w"] / 2, r["y"] + r["h"] / 2
    await pg.mouse("mousePressed", x, y)
    await pg.mouse("mouseReleased", x, y)
    return True


async def key(pg: Page, k: str, modifiers: int = MOD) -> None:
    """A key with the Home's mod held (`Input.dispatchKeyEvent`), sent once the
    Home's document has the focus; logged (`pg.key_log`) with what held the
    focus before and whether the Home's window saw the keydown."""
    focus = await pg.js(FOCUS_HOME) or {}
    code = f"Digit{k}" if k.isdigit() else f"Key{k.upper()}"
    vk = ord(k.upper())
    for t in ("keyDown", "keyUp"):
        await pg.cdp("Input.dispatchKeyEvent", type=t, key=k, code=code, windowsVirtualKeyCode=vk, modifiers=modifiers)
    keys = await pg.js("window.__keys || []") or []
    getattr(pg, "key_log", []).append({"key": k, **focus, "seen_now": keys[focus.get("seen", 0):]})


def page_errors(pg: Page) -> list[str]:
    return [e for e in pg.logs if e.startswith(("EXC", "ERR"))]
