"""The DEV SURFACE shared by E55 (an AI builds it) and E56 (a reference one):
a tiny project with a build step and a static server, the exec agent that may
run exactly those, and the REACTIVE CHECK both score it with."""

from __future__ import annotations

import asyncio
import json
import re
import shlex
import sys
from pathlib import Path

#: Both steps run THIS interpreter (absolute: no `sh`, no `python3` needed on
#: any PATH — Windows has neither).
PY = sys.executable
BUILD_PY = """import pathlib
title = pathlib.Path("app/title.txt").read_text()
pathlib.Path("dist").mkdir(exist_ok=True)
pathlib.Path("dist/index.html").write_text(f'<h1 id="t">{title}</h1>')
print(f"built: {title}", flush=True)
"""


def build_argv() -> list[str]:
    """The build step: `PY build.py`."""
    return [PY, "build.py"]


def run_argv(port: int) -> list[str]:
    """The static server on `port`, serving `dist` — unbuffered, so its
    "Serving HTTP" line reaches the run log the moment it listens. Bound to
    loopback, where the app frame loads it: on all interfaces (`::`) it
    reverse-resolves that address before it prints, which a macOS runner's
    resolver stalls on past the check's wait."""
    return [PY, "-u", "-m", "http.server", str(port), "--bind", "127.0.0.1", "--directory", "dist"]


def write_project(proj: Path, title: str = "Hello v1") -> None:
    (proj / "app").mkdir(exist_ok=True)
    (proj / "app" / "title.txt").write_text(title)
    (proj / "build.py").write_text(BUILD_PY)


def exec_record(port: int) -> dict:
    """`dev`: exactly the build step and the static server on `port`."""
    return {
        "id": "dev",
        "handler_module": "exec_bridge.tools",
        "parent_id": "canvas_root",
        "allowed": ["^" + re.escape(shlex.join(a)) + "$" for a in (build_argv(), run_argv(port))],
        "cwd": ".",
        "inherit_env": ["*"],
        "ingress_rule": "allow_all_ingress",
    }


# EVERY FRAME, through CDP: a panel may render SANDBOXED (null origin — an AI
# need not mark its panels trusted), where the page itself cannot look inside.
async def _frames(pg) -> list[dict]:
    """Every PANEL frame — the canvas page itself (the top frame, where the AI's
    own chat text lives) is left out: its words are not the surface's."""
    tree = (await pg.cdp("Page.getFrameTree")) or {}
    out: list[dict] = []

    def walk(n, top):
        if not top:
            out.append(n.get("frame", {}))
        for c in n.get("childFrames", []) or []:
            walk(c, False)

    walk(tree.get("frameTree", {}), True)
    return out


async def _raw(pg, method: str, params: dict, session: str | None = None, timeout: float = 15):
    """One CDP command, optionally on a flattened child SESSION (an out-of-process
    frame) — the runner's `Page.cdp` speaks the top session only."""
    pg.n += 1
    me = pg.n
    msg = {"id": me, "method": method, "params": params}
    if session:
        msg["sessionId"] = session
    await pg.ws.send(json.dumps(msg))
    while True:
        m = json.loads(await asyncio.wait_for(pg.ws.recv(), timeout))
        if m.get("id") == me:
            if "error" in m:
                raise RuntimeError(f"{method}: {m['error']}")
            return m.get("result", {})


async def _oopif_eval(pg, expr: str, url_has: str | None) -> list:
    """`expr` in every OUT-OF-PROCESS frame: a panel rendered SANDBOXED (null
    origin) runs in its own process, absent from the page's frame tree."""
    vals = []
    targets = (await _raw(pg, "Target.getTargets", {})).get("targetInfos", [])
    for t in targets:
        if t.get("type") != "iframe" or (url_has is not None and url_has not in (t.get("url") or "")):
            continue
        try:
            sid = (await _raw(pg, "Target.attachToTarget", {"targetId": t["targetId"], "flatten": True}))["sessionId"]
            r = await _raw(pg, "Runtime.evaluate", {"expression": expr, "returnByValue": True}, session=sid)
            vals.append(((r or {}).get("result") or {}).get("value"))
        except Exception:
            continue
    return vals


async def frame_eval(pg, expr: str, url_has: str | None = None) -> list:
    """`expr` evaluated in every (matching) panel frame — in-process frames in an
    isolated world, out-of-process ones through their own target session."""
    vals = await _oopif_eval(pg, expr, url_has)
    for fr in await _frames(pg):
        if url_has is not None and url_has not in (fr.get("url") or ""):
            continue
        try:
            w = await pg.cdp("Page.createIsolatedWorld", frameId=fr["id"], worldName="probe")
            r = await pg.cdp("Runtime.evaluate", expression=expr, contextId=w["executionContextId"], returnByValue=True)
            vals.append(((r or {}).get("result") or {}).get("value"))
        except Exception:
            continue
    return vals


async def click_button(pg, label: str, secs: float = 8) -> bool:
    """Click the first button whose text (leading icons such as ▶ ignored)
    starts with `label`, in any panel. A
    panel the AI just placed or updated may be reloading its frame: the button
    is waited for (up to `secs`), then clicked once."""
    expr = ("(()=>{const b=[...document.querySelectorAll('button')].find(b=>b.textContent.replace(/^[^\\p{L}\\p{N}]+/u,'').toLowerCase().startsWith("
            f"{json.dumps(label.lower())}));if(b){{b.click();return true}}return false}})()")
    for _ in range(int(secs * 2)):
        if any(await frame_eval(pg, expr)):
            return True
        await asyncio.sleep(0.5)
    return False


async def texts(pg) -> str:
    return "\n".join(v for v in await frame_eval(pg, "document.body ? document.body.innerText : ''") if isinstance(v, str))


async def frame_urls(pg) -> list[str]:
    return [fr.get("url", "") for fr in await _frames(pg)]


async def app_text(pg, port: int) -> str:
    """The running app's text, from whatever frame loads it."""
    return "".join(v for v in await frame_eval(pg, "document.body ? document.body.innerText : ''", url_has=f":{port}") if isinstance(v, str))


async def until(pg, pred, secs: float) -> bool:
    for _ in range(int(secs * 2)):
        if await pred():
            return True
        await asyncio.sleep(0.5)
    return False


async def reactive_check(pg, proj: Path, port: int) -> dict:
    """Build → Run → Stop → edit + Build + Run, each observed on the page.
    RAPID: a check whose click found no button fails at once — nothing waits
    on an effect whose cause never happened; a check that passes returns on its
    first hit."""
    r: dict = {}

    async def after(click: str, key: str, pred, secs: float) -> None:
        r[key] = bool(r[click]) and await until(pg, pred, secs)

    r["build_clicked"] = await click_button(pg, "build")
    await after("build_clicked", "build_logged", lambda: _has(pg, r"built: Hello v1"), 20)
    r["run_clicked"] = await click_button(pg, "run")
    await after("run_clicked", "run_status", lambda: _has(pg, r"(?i)\brunning\b"), 20)
    await after("run_clicked", "app_shown", lambda: _app(pg, port, "Hello v1"), 25)
    if r["run_clicked"] and not r["app_shown"]:
        # what the page holds instead: every frame's url, the app frame's src, the run log
        print("app diag:", json.dumps({
            "frames": [f.get("url") for f in await _frames(pg)],
            "app_src": await pg.js("[...document.querySelectorAll('.agent-frame iframe')].map(f=>{try{return f.contentDocument?.querySelector('#f')?.src||null}catch(e){return 'x-origin'}})"),
            "text": await app_text(pg, port),
            "runlog": [v for v in await frame_eval(pg, "document.getElementById('l')?.textContent || null") if v][:4],
        })[:2000], flush=True)
    r["stop_clicked"] = await click_button(pg, "stop")
    await after("stop_clicked", "stop_status", lambda: _has(pg, r"(?i)\bstopped\b|\bexited\b|\bdone\b"), 20)
    (proj / "app" / "title.txt").write_text("Hello v2")
    r["_rebuild_clicked"] = await click_button(pg, "build")
    await after("_rebuild_clicked", "rebuilt", lambda: _has(pg, r"built: Hello v2"), 20)
    r["_rerun_clicked"] = await click_button(pg, "run")
    await after("_rerun_clicked", "app_changed", lambda: _app(pg, port, "Hello v2"), 25)
    await click_button(pg, "stop")
    for k in ("_rebuild_clicked", "_rerun_clicked"):
        r.pop(k)
    return r


async def _has(pg, pat: str) -> bool:
    return bool(re.search(pat, await texts(pg)))


async def _app(pg, port: int, want: str) -> bool:
    return want in await app_text(pg, port)
