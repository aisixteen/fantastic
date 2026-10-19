"""THE APP E2E: the built desktop app, on a throwaway home, end to end.

    cd src/lib/python && uv run python ../../app/e2e/run.py --app <fantastic-app[.exe]> \\
        [--native-driver <msedgedriver.exe>]

`fantastic[.exe]` (the canvas sidecar) must sit beside the app. Stdlib plus
`websockets`. Prints `E2E-VERDICT: PASS|FAIL`; the run's files are in
`src/app/e2e/runs/<stamp>/`.

Linux and Windows drive the window with WebDriver (`tauri-driver`, which runs
WebKitWebDriver or msedgedriver). macOS has no WKWebView driver: there the app is
launched as a plain process, read on stderr and asked through the home's socket.

Every call to `home` is the page's own path: `user.forward` over
`ws://127.0.0.1:<port>/user/ws`, on the port of the host's run file.
"""

from __future__ import annotations

import argparse
import asyncio
import json
import os
import platform
import secrets
import shutil
import signal
import socket
import subprocess
import sys
import time
import traceback
import urllib.error
import urllib.request
from pathlib import Path

WINDOWS = sys.platform == "win32"
MACOS = sys.platform == "darwin"
LINUX = sys.platform.startswith("linux")
EXE = ".exe" if WINDOWS else ""
HERE = Path(__file__).resolve().parent
# the app's identifier: WebView2 keeps its user data in %LOCALAPPDATA%/<it>/EBWebView
APP_ID = json.loads((HERE.parent / "tauri.conf.json").read_text())["identifier"]

OS, WEBVIEW = (
    ("windows", "webview2") if WINDOWS else ("macos", "wkwebview") if MACOS else ("linux", "webkitgtk")
)

# ── small things ──────────────────────────────────────────────────────────────


class Fail(Exception):
    pass


def need(ok: object, what: str) -> None:
    if not ok:
        raise Fail(what)


def say(*a: object) -> None:
    print("[app-e2e]", *a, flush=True)


def flat_yaml(p: Path) -> dict | None:
    """A flat `key: value` YAML file (a run file), or None while it is absent."""
    try:
        text = p.read_text(encoding="utf-8")
    except (FileNotFoundError, PermissionError):
        return None
    out: dict = {}
    for line in text.splitlines():
        if ":" not in line or line.startswith((" ", "#", "-")):
            continue
        k, v = line.split(":", 1)
        v = v.strip()
        if len(v) >= 2 and v[0] == v[-1] and v[0] in "'\"":
            v = v[1:-1]
        out[k.strip()] = int(v) if v.isdigit() else v
    return out


def pid_alive(pid: int) -> bool:
    """POSIX `kill(pid, 0)`; on Windows the process opens and has not exited."""
    if not WINDOWS:
        try:
            os.kill(pid, 0)
        except ProcessLookupError:
            return False
        except PermissionError:
            return True
        return True
    import ctypes

    k32 = ctypes.WinDLL("kernel32", use_last_error=True)
    k32.OpenProcess.restype = ctypes.c_void_p
    h = k32.OpenProcess(0x1000, False, pid)
    if not h:
        return ctypes.get_last_error() == 5
    code = ctypes.c_ulong()
    ok = k32.GetExitCodeProcess(ctypes.c_void_p(h), ctypes.byref(code))
    k32.CloseHandle(ctypes.c_void_p(h))
    return bool(ok) and code.value == 259


def kill(pid: int) -> None:
    if not pid_alive(pid):
        return
    if WINDOWS:
        subprocess.run(["taskkill", "/F", "/T", "/PID", str(pid)], capture_output=True)
    else:
        try:
            os.kill(pid, signal.SIGKILL)
        except ProcessLookupError:
            pass


def quit_app(pid: int) -> None:
    """Quit the app the way its OS does: SIGTERM (systemd, `cage`, a logout) on
    POSIX; on Windows WM_CLOSE to its windows (the title bar's close). WebDriver's
    own close is no quit: WebKitWebDriver kills the app with no signal it can
    answer (measured: dead within 0.1 s, no exit requested)."""
    if not WINDOWS:
        os.kill(pid, signal.SIGTERM)
        return
    import ctypes
    from ctypes import wintypes

    u32 = ctypes.WinDLL("user32", use_last_error=True)
    found: list[int] = []
    proc = ctypes.WINFUNCTYPE(wintypes.BOOL, wintypes.HWND, wintypes.LPARAM)

    def each(hwnd, _):
        owner = wintypes.DWORD()
        u32.GetWindowThreadProcessId(hwnd, ctypes.byref(owner))
        if owner.value == pid and u32.IsWindowVisible(hwnd):
            found.append(hwnd)
        return True

    u32.EnumWindows(proc(each), 0)
    need(found, f"no window of the app {pid}")
    for hwnd in found:
        u32.PostMessageW(hwnd, 0x0010, 0, 0)  # WM_CLOSE


def exit_status(code: int | None) -> str:
    """How a process ended, as a person reads it (a Windows NTSTATUS in hex)."""
    if code is None:
        return "still running"
    return f"with {code}" + (f" (0x{code & 0xFFFFFFFF:08X})" if WINDOWS and not 0 <= code < 256 else "")


def listing(root: Path, depth: int) -> str:
    """`root`'s entries `depth` levels down, or that it is missing."""
    if not root.exists():
        return f"({root}: missing)"
    out = []
    for p in sorted(root.rglob("*")):
        rel = p.relative_to(root)
        if len(rel.parts) <= depth:
            out.append(f"{rel}{'/' if p.is_dir() else ''}")
    return "\n".join(out) or "(empty)"


def tail(p: Path, n: int = 80) -> str:
    return "\n".join(p.read_text(errors="replace").splitlines()[-n:]) if p.exists() else f"({p.name}: none)"


# Every msedgewebview2.exe with its parent and command line (a browser
# process has no --type), and what each browser process listens on
WEBVIEW2_PS = (
    "$ps = @(Get-CimInstance Win32_Process -Filter \"Name='msedgewebview2.exe'\"); "
    "if ($ps.Count -eq 0) { 'no msedgewebview2.exe process' }; "
    "foreach ($p in $ps) { \"pid $($p.ProcessId), parent $($p.ParentProcessId): $($p.CommandLine)\"; "
    "if ($p.CommandLine -notmatch '--type=') { "
    "Get-NetTCPConnection -State Listen -OwningProcess $p.ProcessId -ErrorAction SilentlyContinue | "
    "ForEach-Object { \"  listens on $($_.LocalAddress):$($_.LocalPort)\" } } }"
)


def webview2_processes() -> str:
    out = subprocess.run(["powershell", "-NoProfile", "-Command", WEBVIEW2_PS], capture_output=True, text=True, timeout=60)
    return (out.stdout + out.stderr).strip()


def http_ok(url: str) -> bool:
    try:
        with urllib.request.urlopen(url, timeout=5) as r:
            return r.status == 200
    except (urllib.error.URLError, OSError):
        return False


def canonical(p: Path) -> str:
    """A folder as a kernel running there names it."""
    return os.path.abspath(p) if WINDOWS else os.path.realpath(p)


async def until(what: str, check, timeout: float, every: float = 0.5):
    """`check()` (sync or async) until it is truthy; its value. Fails naming `what`."""
    end = time.monotonic() + timeout
    last = None
    while True:
        last = check()
        if asyncio.iscoroutine(last):
            last = await last
        if last:
            return last
        if time.monotonic() > end:
            raise Fail(f"{what}: not within {timeout:g} s")
        await asyncio.sleep(every)


# ── the home, through its socket ─────────────────────────────────────────────


async def host_call(port: int, target: str, payload: dict) -> dict:
    """A call the way a page makes one: through `user` (`e2e/runners/canvas.py`)."""
    import websockets

    body = {"type": "forward", "target": target, "payload": payload}
    frame = {"type": "call", "target": "user", "payload": body, "id": secrets.token_hex(6)}
    async with websockets.connect(f"ws://127.0.0.1:{port}/user/ws", origin=f"http://127.0.0.1:{port}") as ws:
        await ws.send(json.dumps(frame))
        while True:
            m = json.loads(await asyncio.wait_for(ws.recv(), 10))
            if m.get("type") in ("reply", "error") and m.get("id") == frame["id"]:
                return m.get("data", m)


class Run:
    def __init__(self, app: Path, root: Path):
        self.app = app
        self.root = root
        # the home kernel creates <home> private itself: never made here
        self.home_dir = root / "home"
        self.alpha = root / "alpha"
        self.beta = root / "beta"
        self.alpha.mkdir(parents=True)
        self.beta.mkdir()
        self.env = {**os.environ, "FANTASTIC_HOME": str(self.home_dir)}
        self.pids: set[int] = set()

    @property
    def run_file(self) -> Path:
        return self.home_dir / "app" / "home.yaml"

    def host(self) -> dict | None:
        return flat_yaml(self.run_file)

    async def home(self, payload: dict) -> dict:
        host = self.host()
        need(host, f"no run file at {self.run_file}")
        r = await host_call(int(host["port"]), "home", payload)
        need(isinstance(r, dict) and "error" not in r, f"home refused {payload}: {r}")
        return r

    async def row(self, path: str) -> dict | None:
        state = await self.home({"type": "list"})
        return next((r for r in state["rows"] if r["path"] == path), None)

    async def row_status(self, path: str, status: str, timeout: float) -> dict:
        async def check():
            r = await self.row(path)
            return r if r and r["status"] == status else None

        return await until(f"{path} {status}", check, timeout)

    def canvas_run(self, folder: Path) -> dict | None:
        return flat_yaml(folder / ".fantastic" / "cli" / "canvas.yaml")

    async def running_canvas(self, folder: Path, timeout: float = 60) -> dict:
        """`folder` listed `running`, its run file naming a live pid whose url answers."""
        await self.row_status(canonical(folder), "running", timeout)
        run = await until(f"{folder.name}'s canvas.yaml", lambda: self.canvas_run(folder), 30)
        need(isinstance(run.get("pid"), int) and pid_alive(run["pid"]), f"{folder.name}: {run}")
        self.pids.add(run["pid"])
        await until(f"{folder.name}'s canvas answers", lambda: http_ok(run["url"]), 30)
        return run

    async def hand_over(self) -> None:
        """A second launch with beta: it exits 0 within 10 s, and the host lists beta."""
        t0 = time.monotonic()
        p = subprocess.run([str(self.app), str(self.beta)], env=self.env, capture_output=True, text=True, timeout=10)
        say(f"hand-over exited {p.returncode} in {time.monotonic() - t0:.1f} s; stderr: {p.stderr.strip()!r}")
        need(p.returncode == 0, f"the second launch exited {p.returncode}: {p.stderr}")
        await until("beta listed", lambda: self.row(canonical(self.beta)), 60)

    async def canvas_survives(self, run: dict) -> None:
        """P is alive and answers; after a reflect on its kernel and 3 s more, still."""
        need(pid_alive(run["pid"]), f"the canvas {run['pid']} died with the app")
        need(http_ok(run["url"]), f"the canvas {run['url']} stopped answering")
        r = await host_call(int(run["port"]), "kernel", {"type": "reflect", "tree": "none"})
        need(isinstance(r, dict) and "error" not in r, f"its kernel's reflect: {r}")
        await asyncio.sleep(3)
        need(pid_alive(run["pid"]) and http_ok(run["url"]), "the canvas stopped 3 s after the app")

    def teardown(self) -> None:
        for f in (self.alpha, self.beta):
            run = self.canvas_run(f)
            if run and isinstance(run.get("pid"), int):
                self.pids.add(run["pid"])
        for pid in self.pids:
            kill(pid)


# ── WebDriver (Linux, Windows) ───────────────────────────────────────────────

ELEMENT = "element-6066-11e4-a52f-4f8afdd6aa18"


class WebDriver:
    """A WebDriver client over urllib: just the commands this run uses."""

    def __init__(self, base: str, run: Run):
        self.base = base
        self.run = run
        self.sid: str | None = None
        # Windows: the app this run launched, and where its output goes
        self.proc: subprocess.Popen | None = None
        self.app_env: dict[str, str] = {}

    def req(self, method: str, path: str, body: dict | None = None, timeout: float = 120):
        data = None if body is None else json.dumps(body).encode()
        rq = urllib.request.Request(self.base + path, data=data, method=method, headers={"Content-Type": "application/json"})
        for attempt in (1, 2):
            try:
                with urllib.request.urlopen(rq, timeout=timeout) as r:
                    return json.loads(r.read() or b"{}").get("value")
            except urllib.error.HTTPError as e:
                raise Fail(f"WebDriver {method} {path}: {e.code} {e.read()[:400]!r}") from None
            except ConnectionResetError as e:
                # tauri-driver now and then fails to send a request on its pooled
                # connection to the native driver (about 1 in 40 back to back, on
                # Linux) and drops ours unanswered: the request never reached the
                # driver, so it is sent once more
                if attempt == 2:
                    raise
                say(f"WebDriver {method} {path}: {e!r}; once more")

    def s(self, path: str) -> str:
        return f"/session/{self.sid}{path}"

    def start(self, app: Path, args: list[str]) -> None:
        if WINDOWS:
            caps = {"browserName": "webview2", "ms:edgeChromium": True, "ms:edgeOptions": {"debuggerAddress": self.launch(app, args)}}
        else:
            caps = {"tauri:options": {"application": str(app), "args": args}}
        self.sid = self.req("POST", "/session", {"capabilities": {"alwaysMatch": caps}})["sessionId"]

    def launch(self, app: Path, args: list[str]) -> str:
        """Windows: start the app with exactly `args`, its WebView2 on a debugger
        port, and return that port's address for msedgedriver to attach to:
        msedgedriver then launches nothing and adds no argv of its own."""
        with socket.socket() as s:
            s.bind(("127.0.0.1", 0))
            port = s.getsockname()[1]
        env = {
            **self.run.env,
            "WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS": f"--remote-debugging-port={port} --enable-logging",
            "TAURI_WEBVIEW_AUTOMATION": "true",
        }
        self.app_env = {k: v for k, v in env.items() if k.upper().startswith(("WEBVIEW2_", "TAURI_"))}
        out = open(self.run.root / "app.out", "a")
        self.proc = subprocess.Popen([str(app), *args], env=env, stdout=out, stderr=subprocess.STDOUT)
        self.run.pids.add(self.proc.pid)
        end = time.monotonic() + 60
        while not http_ok(f"http://127.0.0.1:{port}/json/version"):
            code = self.proc.poll()
            need(code is None, f"the app exited {exit_status(code)} before its WebView2 served port {port}; argv {args}")
            need(time.monotonic() < end, f"no WebView2 on port {port} within 60 s; argv {args}")
            time.sleep(0.25)
        return f"127.0.0.1:{port}"

    def url(self) -> str:
        return self.req("GET", self.s("/url"))

    def js(self, script: str, *args):
        return self.req("POST", self.s("/execute/sync"), {"script": script, "args": list(args)})

    def find(self, css: str) -> str | None:
        found = self.req("POST", self.s("/elements"), {"using": "css selector", "value": css})
        # the W3C key, or the one value of a driver's own reference
        return (found[0].get(ELEMENT) or next(iter(found[0].values()))) if found else None

    def click(self, css: str) -> None:
        el = self.find(css)
        need(el, f"no element {css}")
        self.req("POST", self.s(f"/element/{el}/click"), {})

    def handles(self) -> list:
        return self.req("GET", self.s("/window/handles"))

    def rect(self) -> dict:
        return self.req("GET", self.s("/window/rect"))

    def end(self) -> None:
        if self.sid is not None:
            try:
                self.req("DELETE", self.s(""), timeout=30)
            except (Fail, OSError) as e:
                say(f"end session: {e}")
            self.sid = None
        # an attached driver leaves the app running: the run that launched it ends it
        if self.proc is not None and self.proc.poll() is None:
            kill(self.proc.pid)


SHELL_JS = """const f = globalThis.__FANTASTIC_SHELL__;
return f === undefined ? null : {name: f.name, os: f.os, webview: f.webview, backdrop: f.backdrop,
  pickFolder: typeof f.pickFolder, frozen: Object.isFrozen(f)};"""

# The Home's carousel at rest on the Home: no slide running, at x 0.
CAROUSEL_STILL_JS = """const c = document.querySelector('#home .carousel');
return c !== null && c.getAnimations().length === 0 && new DOMMatrix(getComputedStyle(c).transform).m41 === 0;"""


def windows_build() -> int:
    return sys.getwindowsversion().build if WINDOWS else 0


async def on_page(wd: WebDriver, run: Run) -> dict:
    """The window shows the run file's url within 60 s."""
    host = await until("the host's run file", run.host, 60)
    await until(f"the window on {host['url']}", lambda: wd.url() == host["url"], 60)
    return host


async def webdriver(run: Run, args: argparse.Namespace) -> None:
    cmd = ["tauri-driver", "--port", "4444"]
    if args.native_driver:
        cmd += ["--native-driver", args.native_driver]
    log = open(run.root / "tauri-driver.log", "w")
    driver = subprocess.Popen(cmd, env=run.env, stdout=log, stderr=subprocess.STDOUT)
    wd = WebDriver("http://127.0.0.1:4444", run)
    try:
        await sessions(run, wd)
    except BaseException:
        log.flush()
        say("the driver's log, last 80 lines (on Linux the app's stderr is in it):")
        print(tail(run.root / "tauri-driver.log"), flush=True)
        if WINDOWS and wd.proc is not None:
            say(f"the app, launched by this run (pid {wd.proc.pid}): {exit_status(wd.proc.poll())}")
            print(tail(run.root / "app.out"), flush=True)
            say(f"the app's WEBVIEW2_* and TAURI_* environment: {wd.app_env}")
            data = Path(os.environ["LOCALAPPDATA"]) / APP_ID
            say(f"the app's data dir ({data}), two levels:")
            print(listing(data, 2), flush=True)
            webview_log = data / "EBWebView" / "chrome_debug.log"
            say(f"WebView2's log ({webview_log}), last 80 lines:")
            print(tail(webview_log), flush=True)
            say("every msedgewebview2.exe (parent, command line) and the browser processes' listening ports:")
            print(webview2_processes(), flush=True)
        raise
    finally:
        wd.end()
        driver.terminate()
        try:
            driver.wait(10)
        except subprocess.TimeoutExpired:
            driver.kill()
        log.close()


async def sessions(run: Run, wd: WebDriver) -> None:
    """Steps 1–8 (8: Linux only), on one driver."""
    await until("tauri-driver up", lambda: _status(wd), 30)
    alpha = canonical(run.alpha)

    say("1. a session with alpha: the window shows the Home page")
    wd.start(run.app, [str(run.alpha)])
    host = await on_page(wd, run)
    app_pid = int(host["pid"])

    say("2. the shell's flag")
    flag = wd.js(SHELL_JS)
    backdrop = "native" if WINDOWS and windows_build() >= 22000 else "web"
    need(flag == {"name": "tauri", "os": OS, "webview": WEBVIEW, "backdrop": backdrop, "pickFolder": "function", "frozen": True}, f"the flag: {flag}")

    say("3. the argv intent: alpha runs")
    p = await run.running_canvas(run.alpha)

    say("4. through the page: stop, start, select")
    row = (await run.row(alpha))["id"]
    sel = f'.home-row[data-id="{row}"]'
    await show_home(wd, run)
    await until("alpha's row on the page", lambda: wd.find(sel), 30)
    wd.click(f'{sel} button[data-intent="stop"]')
    await run.row_status(alpha, "stopped", 30)
    await until("alpha's run file gone", lambda: run.canvas_run(run.alpha) is None, 30)
    await until("alpha's start button", lambda: wd.find(f'{sel} button[data-intent="start"]'), 15)
    wd.click(f'{sel} button[data-intent="start"]')
    p = await run.running_canvas(run.alpha, 30)
    await until("alpha's row again", lambda: wd.find(f"{sel} .path"), 15)
    wd.click(f"{sel} .path")
    await until("alpha selected", lambda: _selected(run, row), 30)
    frame = f'.page[data-id="{row}"] iframe.canvas-frame'
    src = await until("alpha's canvas frame", lambda: wd.js("const f = document.querySelector(arguments[0]); return f && f.src;", frame), 30)
    need(src == p["url"], f"the frame shows {src}, not {p['url']}")

    say("5. the hand-over: a second launch with beta")
    await run.hand_over()
    handles = wd.handles()
    need(len(handles) == 1, f"a second window: {handles}")

    say("6. the orderly quit: the OS's own (SIGTERM on Linux, WM_CLOSE on Windows)")
    quit_app(app_pid)
    await until("the app gone", lambda: not pid_alive(app_pid), 20)
    need(not run.run_file.exists(), "the run file outlived the quit: no orderly shutdown")
    await run.canvas_survives(p)
    wd.end()

    say("7. a second session, no args: alpha is adopted, and stops")
    wd.start(run.app, [])
    await on_page(wd, run)
    await run.row_status(alpha, "running", 60)
    await show_home(wd, run)
    await until("alpha's row on the page", lambda: wd.find(sel), 30)
    wd.click(f'{sel} button[data-intent="stop"]')
    await run.row_status(alpha, "stopped", 30)
    await until(f"the canvas {p['pid']} gone", lambda: not pid_alive(p["pid"]), 30)
    app_pid = int(run.host()["pid"])
    quit_app(app_pid)
    await until("the app gone", lambda: not pid_alive(app_pid), 20)
    need(not run.run_file.exists(), "the run file outlived the quit")
    wd.end()

    if LINUX:
        say("8. kiosk: fullscreen, no folder dialog")
        wd.start(run.app, ["--kiosk"])
        await on_page(wd, run)
        rect = await until("a fullscreen window", lambda: _fullscreen(wd), 20)
        need(rect, f"the kiosk window: {rect}")
        flag = wd.js(SHELL_JS)
        need(flag and flag["pickFolder"] == "undefined", f"the kiosk's flag: {flag}")
        await until("the Home page loads", lambda: wd.js("return document.body && document.body.dataset.page"), 30)
        app_pid = int(run.host()["pid"])
        quit_app(app_pid)
        await until("the app gone", lambda: not pid_alive(app_pid), 20)
        need(not run.run_file.exists(), "the kiosk's run file outlived the quit")
        wd.end()


def _status(wd: WebDriver) -> bool:
    try:
        wd.req("GET", "/status", timeout=3)
        return True
    except (Fail, OSError):
        return False


async def show_home(wd: WebDriver, run: Run) -> None:
    """The nav pill's Home: an `open` selects the page it opened, and a restart
    restores the selection, so the Home is not what shows."""
    await until("the nav pill's Home", lambda: wd.find("nav.nav-pill .nav-home"), 30)
    wd.click("nav.nav-pill .nav-home")
    await until("the Home shown", lambda: _selected(run, None), 30)
    # the carousel slides to the Home; a click before it lands is off-screen
    await until("the Home slid in", lambda: wd.js(CAROUSEL_STILL_JS), 15)


async def _selected(run: Run, row: str | None) -> bool:
    return (await run.home({"type": "list"}))["selection"] == row


def _fullscreen(wd: WebDriver):
    r = wd.rect()
    return r if (r.get("width"), r.get("height")) == (1280, 800) else None


# ── macOS (no WebDriver) ─────────────────────────────────────────────────────


def launch(run: Run, args: list[str], name: str) -> tuple[subprocess.Popen, Path]:
    err = run.root / f"{name}.stderr"
    p = subprocess.Popen([str(run.app), *args], env=run.env, stdout=subprocess.DEVNULL, stderr=open(err, "w"))
    return p, err


async def announced(p: subprocess.Popen, err: Path) -> dict:
    def lines():
        need(p.poll() is None, f"the app exited {p.returncode}: {err.read_text()}")
        text = err.read_text()
        got = {}
        for line in text.splitlines():
            for key in ("home", "backdrop"):
                pre = f"fantastic-app: the {key}: "
                if line.startswith(pre):
                    got[key] = line[len(pre):].strip()
        return got if len(got) == 2 else None

    return await until("the app's announcements", lines, 60)


async def macos(run: Run) -> None:
    alpha = canonical(run.alpha)

    say("1. launch with alpha: the home and the backdrop on stderr")
    app, err = launch(run, [str(run.alpha)], "first")
    try:
        got = await announced(app, err)
        host = await until("the host's run file", run.host, 10)
        need(got["home"] == host["url"], f"announced {got['home']}, the run file says {host['url']}")
        want = "liquid-glass" if int(platform.mac_ver()[0].split(".")[0]) >= 26 else "vibrancy"
        need(got["backdrop"] == want, f"the backdrop: {got['backdrop']}, want {want}")

        say("3. the argv intent: alpha runs")
        p = await run.running_canvas(run.alpha)

        say("4. through the home: stop, start, select")
        row = (await run.row(alpha))["id"]
        await run.home({"type": "stop", "id": row})
        await run.row_status(alpha, "stopped", 30)
        await run.home({"type": "start", "id": row})
        p = await run.running_canvas(run.alpha, 30)
        state = await run.home({"type": "select", "id": row})
        need(state["selection"] == row, f"select: {state['selection']}")

        say("5. the hand-over")
        await run.hand_over()

        say("6. the orderly quit: SIGTERM")
        app.send_signal(signal.SIGTERM)
        code = await asyncio.to_thread(app.wait, 20)
        need(code == 0, f"the app exited {code} on SIGTERM: {err.read_text()}")
        need(not run.run_file.exists(), "the run file outlived SIGTERM: no orderly shutdown")
        await run.canvas_survives(p)
    finally:
        if app.poll() is None:
            app.kill()

    say("7. the crash path: relaunch, SIGKILL; the next launch boots and adopts")
    app, err = launch(run, [], "crashed")
    try:
        await announced(app, err)
        app.kill()
        app.wait(10)
        need(run.run_file.exists(), "a killed app left no run file (it cannot clean up)")
    finally:
        if app.poll() is None:
            app.kill()
    app, err = launch(run, [], "after-crash")
    try:
        await announced(app, err)
        await run.row_status(alpha, "running", 60)
        row = (await run.row(alpha))["id"]
        await run.home({"type": "stop", "id": row})
        await run.row_status(alpha, "stopped", 30)
        await until(f"the canvas {p['pid']} gone", lambda: not pid_alive(p["pid"]), 30)
        app.send_signal(signal.SIGTERM)
        await asyncio.to_thread(app.wait, 20)
    finally:
        if app.poll() is None:
            app.kill()


# ── main ─────────────────────────────────────────────────────────────────────


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("--app", required=True, type=Path)
    ap.add_argument("--native-driver", help="msedgedriver.exe (Windows)")
    args = ap.parse_args()
    app = args.app.resolve()
    need(app.is_file(), f"no app at {app}")
    need((app.parent / f"fantastic{EXE}").is_file(), f"no fantastic{EXE} beside {app}")
    if not MACOS:
        need(shutil.which("tauri-driver"), "tauri-driver is not on PATH")
    root = HERE / "runs" / time.strftime("%Y%m%d-%H%M%S")
    root.mkdir(parents=True)
    run = Run(app, root)
    say(f"app {app}; run {root}")
    ok = False
    try:
        asyncio.run(macos(run) if MACOS else webdriver(run, args))
        ok = True
    except Exception as e:  # noqa: BLE001 — every failure is the verdict's
        say("FAILED:", e)
        traceback.print_exc()
    finally:
        run.teardown()
    print(f"E2E-VERDICT: {'PASS' if ok else 'FAIL'}", flush=True)
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main())
