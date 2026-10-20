"""THE WEBKITGTK CLIENT (E88, #52) — a page in a real WebKitGTK webview, with
the method names the Chrome `Page` has (`js`, `until`), so one measurement
serves both. It runs in the e2e venv; the webview runs in `webkitgtk_host.py`
under the SYSTEM python, where python3-gi and WebKit2 4.1 live.

`SYSTEM_PY` is `/usr/bin/python3`; `E2E_SYSTEM_PYTHON` names another (a machine
whose /usr/bin/python3 is not the distro's, so its gi does not load)."""

from __future__ import annotations

import asyncio
import json
import os
import shutil
import signal
import subprocess
import time
from pathlib import Path

HOST = Path(__file__).resolve().parent / "webkitgtk_host.py"
#: the host's protocol lines start with it (`webkitgtk_host.MARK`)
MARK = "@webkitgtk "
SYSTEM_PY = os.environ.get("E2E_SYSTEM_PYTHON") or "/usr/bin/python3"
INSTALL = "install python3-gi gir1.2-webkit2-4.1 xvfb"
#: how long the webview has to load the page and say it is ready
READY_S = 60.0


def missing() -> str | None:
    """Why this machine cannot host WebKitGTK, or None when it can."""
    r = subprocess.run([SYSTEM_PY, "-I", "-c", "import gi; gi.require_version('WebKit2', '4.1'); from gi.repository import WebKit2"],
                       capture_output=True, text=True)
    if r.returncode != 0:
        return f"{INSTALL} ({SYSTEM_PY}: {(r.stderr.strip().splitlines() or ['no WebKit2 4.1'])[-1]})"
    if not (os.environ.get("DISPLAY") or os.environ.get("WAYLAND_DISPLAY")) and shutil.which("xvfb-run") is None:
        return f"{INSTALL} (no display and no xvfb-run)"
    return None


class WebKitGtkPage:
    """One WebKitGTK window on one page."""

    def __init__(self) -> None:
        self.proc: asyncio.subprocess.Process | None = None
        self.n = 0
        self.ua: str | None = None
        self.logs: list[str] = []
        self.misses: list[str] = []
        self._waiting: dict[int, asyncio.Future] = {}
        self._ready: asyncio.Future | None = None
        self._reader: asyncio.Task | None = None
        self.t0 = time.time()

    async def start(self, url: str, init_script: str | None = None, width: int = 1400, height: int = 900) -> None:
        args = [SYSTEM_PY, "-I", str(HOST), "--url", url, "--width", str(width), "--height", str(height)]
        if init_script is not None:
            args += ["--init-script", init_script]
        if not (os.environ.get("DISPLAY") or os.environ.get("WAYLAND_DISPLAY")):
            args = ["xvfb-run", "-a", *args]
        # the webview's own defaults, as the Tauri shell has them: no WEBKIT_* variables
        env = {k: v for k, v in os.environ.items() if not k.startswith("WEBKIT_")}
        env["NO_AT_BRIDGE"] = "1"
        self.proc = await asyncio.create_subprocess_exec(*args, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE, env=env,
                                                         limit=16 * 1024 * 1024, start_new_session=True)
        self._ready = asyncio.get_running_loop().create_future()
        self._reader = asyncio.create_task(self._read())
        self._err = asyncio.create_task(self._read_err())
        ready = await asyncio.wait_for(self._ready, READY_S)
        if not ready.get("ready"):
            raise RuntimeError(f"webkitgtk: the page never loaded {url}: {ready.get('error')}")
        self.ua = ready.get("ua")
        self.t0 = time.time()

    async def _read(self) -> None:
        assert self.proc is not None and self.proc.stdout is not None
        while line := await self.proc.stdout.readline():
            text = line.decode(errors="replace").rstrip("\n")
            if not text.startswith(MARK):
                self.logs.append(text)
                continue
            msg = json.loads(text[len(MARK):])
            if "ready" in msg:
                if self._ready is not None and not self._ready.done():
                    self._ready.set_result(msg)
                continue
            fut = self._waiting.pop(msg.get("id"), None)
            if fut is not None and not fut.done():
                fut.set_result(msg)
        code = await self.proc.wait()
        why = RuntimeError(f"webkitgtk: the host exited ({code}): {' | '.join(self.logs[-5:])}")
        if self._ready is not None and not self._ready.done():
            self._ready.set_exception(why)
        for fut in self._waiting.values():
            if not fut.done():
                fut.set_exception(why)
        self._waiting.clear()

    async def _read_err(self) -> None:
        assert self.proc is not None and self.proc.stderr is not None
        while line := await self.proc.stderr.readline():
            self.logs.append(line.decode(errors="replace").rstrip())

    async def js(self, expr: str):
        assert self.proc is not None and self.proc.stdin is not None
        self.n += 1
        me = self.n
        fut = asyncio.get_running_loop().create_future()
        self._waiting[me] = fut
        self.proc.stdin.write((json.dumps({"id": me, "js": expr}) + "\n").encode())
        await self.proc.stdin.drain()
        msg = await asyncio.wait_for(fut, 30)
        if "error" in msg:
            raise RuntimeError(f"webkitgtk: {msg['error']} in {' '.join(expr.split())[:160]}")
        return msg.get("value")

    async def until(self, expr: str, timeout: float = 10.0) -> bool:
        deadline = time.time() + timeout
        while time.time() < deadline:
            if await self.js(expr):
                return True
            await asyncio.sleep(0.1)
        self.misses.append(f"+{time.time() - self.t0:.1f}s {timeout:g}s {' '.join(expr.split())[:160]}")
        return False

    async def close(self) -> None:
        """Quit the window, then stop its whole process group: `xvfb-run` leaves
        its X server, and WebKit its own processes, when only it is stopped."""
        if self.proc is None:
            return
        if self.proc.returncode is None and self.proc.stdin is not None:
            try:
                self.proc.stdin.write(b'{"cmd": "quit"}\n')
                await self.proc.stdin.drain()
                await asyncio.wait_for(self.proc.wait(), 10)
            except (asyncio.TimeoutError, BrokenPipeError, ConnectionResetError):
                pass
        try:
            os.killpg(self.proc.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
        await self.proc.wait()
        for t in (self._reader, self._err):
            if t is not None:
                await t
        self.proc = None
