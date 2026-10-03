"""The DEV SURFACE shared by E55 (an AI builds it) and E56 (a reference one):
a tiny project with a build step and a static server, the exec agent that may
run exactly those, and the REACTIVE CHECK both score it with."""

from __future__ import annotations

import asyncio
import json
import re
from pathlib import Path

BUILD_SH = """mkdir -p dist
printf '<h1 id="t">%s</h1>' "$(cat app/title.txt)" > dist/index.html
echo "built: $(cat app/title.txt)"
"""


def write_project(proj: Path, title: str = "Hello v1") -> None:
    (proj / "app").mkdir(exist_ok=True)
    (proj / "app" / "title.txt").write_text(title)
    (proj / "build.sh").write_text(BUILD_SH)


def exec_record(port: int) -> dict:
    """`dev`: exactly `sh build.sh` and the static server on `port`."""
    return {
        "id": "dev",
        "handler_module": "exec_bridge.tools",
        "parent_id": "view_root",
        "allowed": [r"^sh build\.sh$", rf"^python3 -m http\.server {port} --directory dist$"],
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


async def click_button(pg, label: str) -> bool:
    expr = ("(()=>{const b=[...document.querySelectorAll('button')].find(b=>b.textContent.trim().toLowerCase().startsWith("
            f"{json.dumps(label.lower())}));if(b){{b.click();return true}}return false}})()")
    return any(await frame_eval(pg, expr))


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
    """Build → Run → Stop → edit + Build + Run, each observed on the page."""
    r: dict = {}
    r["build_clicked"] = await click_button(pg, "build")
    r["build_logged"] = await until(pg, lambda: _has(pg, r"built: Hello v1"), 20)
    r["run_clicked"] = await click_button(pg, "run")
    r["run_status"] = await until(pg, lambda: _has(pg, r"(?i)\brunning\b"), 20)
    r["app_shown"] = await until(pg, lambda: _app(pg, port, "Hello v1"), 25)
    r["stop_clicked"] = await click_button(pg, "stop")
    r["stop_status"] = await until(pg, lambda: _has(pg, r"(?i)\bstopped\b|\bexited\b|\bdone\b"), 20)
    (proj / "app" / "title.txt").write_text("Hello v2")
    await click_button(pg, "build")
    r["rebuilt"] = await until(pg, lambda: _has(pg, r"built: Hello v2"), 20)
    await click_button(pg, "run")
    r["app_changed"] = await until(pg, lambda: _app(pg, port, "Hello v2"), 25)
    await click_button(pg, "stop")
    return r


async def _has(pg, pat: str) -> bool:
    return bool(re.search(pat, await texts(pg)))


async def _app(pg, port: int, want: str) -> bool:
    return want in await app_text(pg, port)
