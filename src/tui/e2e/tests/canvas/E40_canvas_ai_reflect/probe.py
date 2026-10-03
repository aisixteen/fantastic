"""E40 canvas_ai_reflect — does an AI cell find the page kernel on its own?

See README.md. The model comes from the plug (`BACKEND=…`, e2e/backends); the
fixture is the PRODUCT (`fantastic --headless canvas`) driven through a real
headless Chrome — nothing here builds agents directly.
"""

from __future__ import annotations

import asyncio
import json
import os
import re
import shutil
import signal
import subprocess
import sys
import tempfile
import time
import urllib.request
from pathlib import Path

import yaml

HERE = Path(__file__).resolve().parent
REPO = next(d for d in HERE.parents if (d / "e2e" / ".e2e-root").exists())
sys.path.insert(0, str(REPO / "e2e"))
sys.path.insert(0, str(REPO / "e2e" / "runners"))

import websockets  # noqa: E402

from backends import backend  # noqa: E402
from canvas import MAC_CHROME, Page, free_port  # noqa: E402
from helpers.verdict import verdict  # noqa: E402

SRC = REPO / "src"
BIN = SRC / "target" / "debug" / "fantastic"
#: The backends the CLI links, and the cargo feature that links each.
sys.path.insert(0, str(HERE.parent.parent / "_lib"))
from canvas_live import FEATURES, seed_key  # noqa: E402
FIRST_TURN = "Hey! Look around and see whats on a canvas and a backend"

LIMITS = (yaml.safe_load((HERE / "test.yaml").read_text()) or {}).get("limits") or {}
TOTAL = float(LIMITS["total_seconds"])
QUIET_S = float(LIMITS["quiet_seconds"])


def build(feature: str, into: Path) -> Path:
    r = subprocess.run(["cargo", "build", "-p", "fantastic-tui", "--features", feature], cwd=SRC, capture_output=True, text=True)
    if r.returncode != 0:
        raise SystemExit(f"cargo build --features {feature} failed:\n{r.stderr[-2000:]}")
    b = into / "fantastic"
    shutil.copy2(BIN, b)  # a private copy: another build cannot swap it mid-run
    return b


def private_write(p: Path, text: str) -> None:
    fd = os.open(p, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
    os.write(fd, text.encode())
    os.close(fd)


async def spot(pg: Page, x: int, y: int, text: str) -> None:
    for c in (1, 2):
        await pg.mouse("mousePressed", x, y, c)
        await pg.mouse("mouseReleased", x, y, c)
    await pg.until("!!document.querySelector('.action-popup input')", 10)
    await pg.cdp("Input.insertText", text=text)
    await asyncio.sleep(0.3)
    await pg.cdp("Input.dispatchKeyEvent", type="keyDown", key="Enter", code="Enter", windowsVirtualKeyCode=13)


async def drive(url: str, cdp: int) -> dict:
    for _ in range(50):
        try:
            tabs = json.loads(urllib.request.urlopen(f"http://127.0.0.1:{cdp}/json/list").read())
            break
        except Exception:
            time.sleep(0.2)
    ws_url = next(t for t in tabs if t["type"] == "page")["webSocketDebuggerUrl"]
    async with websockets.connect(ws_url, max_size=None) as ws:
        pg = Page(ws)
        await pg.cdp("Runtime.enable")
        await pg.cdp("Page.enable")
        await pg.open(url)
        await pg.until("!!document.getElementById('canvas')", 30)
        await asyncio.sleep(1.5)
        # the neighbourhood: a terminal to the right, an html panel below it
        await spot(pg, 1150, 250, "terminal")
        await pg.until("!!document.querySelector('.agent-frame .xterm')", 20)
        await spot(pg, 1150, 650, "html")
        await asyncio.sleep(1.0)
        # the AI cell, at the left, asked in plain words
        await spot(pg, 350, 300, FIRST_TURN)
        await pg.until("!!document.querySelector('.chat-view')", 20)
        # wait out the dialog: an answer in words, then QUIET — no new message
        # for QUIET_S (a model may keep calling after its first words)
        deadline = time.time() + TOTAL
        done, last_n, quiet_since = False, -1, time.time()
        while time.time() < deadline:
            n = await pg.js("document.querySelectorAll('.chat-view .msg').length") or 0
            said = await pg.js("[...document.querySelectorAll('.chat-view .msg.assistant')].some(m=>m.textContent.trim())")
            if n != last_n:
                last_n, quiet_since = n, time.time()
            elif said and time.time() - quiet_since >= QUIET_S:
                done = True
                break
            await asyncio.sleep(2)
        msgs = await pg.js("[...document.querySelectorAll('.chat-view .msg')].map(m=>[m.className, m.textContent])")
        return {"done": bool(done), "msgs": msgs or [], "errors": [e for e in pg.logs if "WebGL" not in e][:6]}


def main() -> bool:
    be = backend()
    if be.blockers():
        return verdict(False, "the plug cannot run: " + "; ".join(be.blockers()))
    feature = FEATURES.get(be.handler_module)
    if feature is None:
        return verdict(False, f"the CLI links no {be.handler_module} — it links {', '.join(FEATURES)}")
    tmp = Path(tempfile.mkdtemp(prefix="fc-e40-"))
    binary = build(feature, tmp)
    # the user's home: ONE template, the plug's own record, made the default
    home = tmp / "home"
    (home / "templates").mkdir(parents=True)
    os.chmod(home, 0o700)
    os.chmod(home / "templates", 0o700)
    private_write(home / "templates" / "plug.yaml", yaml.safe_dump(seed_key(home, be.record()), sort_keys=False))
    private_write(home / "templates" / ".preferred", "plug")
    proj = tmp / "proj"
    proj.mkdir()
    log = tmp / "host.log"
    env = {**os.environ, "FANTASTIC_HOME": str(home)}
    host = subprocess.Popen([str(binary), "--headless", "canvas", "--port", str(free_port())], cwd=proj, env=env,
                            stdout=open(log, "w"), stderr=subprocess.STDOUT)
    chrome = None
    try:
        url = None
        for _ in range(300):
            if host.poll() is not None:
                return verdict(False, f"the host exited {host.returncode}: {log.read_text()[-600:]}")
            m = re.search(r"^fantastic: the canvas: (\S+)", log.read_text(), re.M)
            if m:
                url = m.group(1)
                break
            time.sleep(0.2)
        if url is None:
            return verdict(False, "the host never announced the canvas")
        cdp = free_port()
        chrome = subprocess.Popen([MAC_CHROME, "--headless=new", "--window-size=1500,950", f"--remote-debugging-port={cdp}",
                                   f"--user-data-dir={tmp / 'chrome'}", "--no-first-run", "about:blank"],
                                  stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        out = asyncio.run(drive(url, cdp))
    finally:
        if chrome is not None:
            chrome.terminate()
        host.send_signal(signal.SIGTERM)
        try:
            host.wait(30)
        except subprocess.TimeoutExpired:
            host.kill()
    # ── what the AI did, from its own cell (the tool lines) and the host stream
    root = yaml.safe_load((proj / ".fantastic" / "agent.yaml").read_text()) or {}
    host_guid = root.get("guid", "")
    canvas_rec = yaml.safe_load((proj / ".fantastic/web/agents/canvas/agent.yaml").read_text()) or {}
    page_guid = canvas_rec.get("guid", "")
    # THE EVIDENCE is the host's own stream: every call to a PAGE agent is a
    # `[<id>] <page guid>|<agent> call <verb>` line (only a cell's AI holds peer
    # grants). The cell's `→` tool lines are a view of it — a turn that mixes
    # words with several calls shows none — so they are reported, not counted.
    calls = [t for c, t in out["msgs"] if "tool" in c and t.startswith("→")]
    stream = [line for line in log.read_text().splitlines()
              if page_guid and re.match(rf"^\[\d+\] {re.escape(page_guid)}\|\S+ call \S+", line)]
    on_page = [line.split(" ", 1)[1] for line in stream]
    reflects = [c for c in on_page if c.endswith(" call reflect")]
    answer = next((t for c, t in reversed(out["msgs"]) if "assistant" in c and t.strip()), "")
    print(f"page guid {page_guid} · host guid {host_guid}")
    print(f"dialog finished: {out['done']} · turns shown: {len(out['msgs'])}")
    print("the cell's tool lines:")
    for c in calls:
        print(f"  {c}")
    print(f"page calls in the host stream: {len(stream)}")
    for line in stream[:16]:
        print(f"  {line}")
    # `around` is the host's (`user.around`, from the page records)
    print(f"called around: {any(re.match(r'^\[\d+\] user call around', l) for l in log.read_text().splitlines())}")
    print(f"answer names a terminal: {'terminal' in answer.lower()} · an html panel: {'html' in answer.lower()}")
    print(f"answer: {answer[:600]}")
    if out["errors"]:
        print(f"page errors: {out['errors']}")
    shutil.rmtree(tmp, ignore_errors=True)
    if not page_guid:
        return verdict(False, "the page never minted its guid — it is no peer")
    return verdict(bool(reflects), f"{len(reflects)} reflect(s) on the page" if reflects else "the AI never reflected an agent of the page")


if __name__ == "__main__":
    sys.exit(0 if main() else 1)
