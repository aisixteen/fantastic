"""E61 canvas_cells — the canvas with nothing floating (owner 2026-10-08):

a) a fresh canvas is its background alone — no top bar, no sidebar, no cell;
b) the double-click's `host · coding agents` group lists a coding agent found on
   PATH (`user path`), with its full path — a FAKE `claude` this probe puts
   first on PATH (no real agent runs);
c) its tile starts EXACTLY that program in a terminal cell, interactive (it
   echoes what is typed);
d) `file tree` is a cell placed from the double-click (the canvas-list cell is
   gone from the menu for now, owner 2026-10-09); every agent is a folder in it;
e) `new_canvas` on the canvas node makes a PEER canvas, opened by its URL: its
   own page records and guid, empty, and its file tree marks the first canvas's
   agents as on another canvas (`.elsewhere`);
f) a page bundle in that canvas's own folder runs its instance, and an edit of
   its bundle.yaml re-renders it live;
g) `--headless canvas` is refused (`canvas` is headless already) and leaves the
   directory untouched — no `.fantastic`."""

from __future__ import annotations

import asyncio
import os
import re
import shlex
import subprocess
import sys
from pathlib import Path

import yaml

sys.path.insert(0, str(Path(__file__).resolve().parents[2]))
from _lib.canvas_live import Rig, verdict, open_tree, spot  # noqa: E402
from helpers import oslayer  # noqa: E402

FAKE = "claude"
MARK = "FAKE-CODER-READY"
PEER = "canvas_second"


def main() -> bool:
    rig = Rig(__file__, live=False)
    rig.build()
    rig.make_home(None)
    # the fake coding agent, FIRST on the host's PATH
    fake_dir = rig.proj.parent / "fakebin"
    fake_dir.mkdir(parents=True, exist_ok=True)
    fake = oslayer.fake_command(fake_dir, FAKE, MARK)
    os.environ["PATH"] = f"{fake_dir}{os.pathsep}{os.environ.get('PATH', '')}"
    # g) both flags together: refused before anything is written
    bare = rig.proj.parent / "refuse"
    bare.mkdir(parents=True, exist_ok=True)
    both = subprocess.run([str(rig.binary), "--headless", "canvas"], cwd=bare, env=rig.env(), capture_output=True, text=True, timeout=30)
    refused = both.returncode != 0 and not (bare / ".fantastic").exists()
    rig.init()
    from _lib.dev_surface import frame_eval

    r: dict = {"headless_canvas_refused": refused}
    secs = rig.limits["seconds"]
    frames = "[...document.querySelectorAll('.agent-frame')]"

    async def tile(pg, x: int, y: int, label: str) -> bool:
        for c in (1, 2):
            await pg.mouse("mousePressed", x, y, c)
            await pg.mouse("mouseReleased", x, y, c)
        await pg.until("!!document.querySelector('.action-popup .tile')", 10)
        await asyncio.sleep(1.0)  # the coding agents come with the presets
        return bool(await pg.js(f"(()=>{{const t=[...document.querySelectorAll('.action-popup .tile')].find(e=>(e.querySelector('.lb')||{{}}).textContent==={label!r}); if(!t) return false; t.dispatchEvent(new MouseEvent('mousedown',{{bubbles:true}})); return true}})()"))

    async def reveal(guid: str, cell: str, agent: str, secs_: float = 10) -> bool:
        """Show agent `agent`'s folder in the tree cell `cell`, through the cell's own
        verb (`<guid>|<cell>`). The agent folders are read from disk by a handful of
        bridge `list` calls after the tree mounts, so a first `not_found` is asked
        again for at most `secs_` (10 s: a slower walk is a failure); any other
        refusal ends it."""
        from canvas import host_call

        end = asyncio.get_event_loop().time() + secs_
        while asyncio.get_event_loop().time() < end:
            a = await host_call(rig.port, f"{guid}|{cell}", {"type": "reveal", "agent": agent})
            if "revealed" in a:
                return True
            if a.get("reason") != "not_found":
                return False
            await asyncio.sleep(0.5)
        return False

    async def first() -> None:
        async with rig.page(1600, 1000) as pg:
            await asyncio.sleep(2)
            # a) nothing floats
            r["bare"] = await pg.js(f"!document.getElementById('toolbar') && !document.getElementById('chats') && {frames}.length === 0")
            # b) the coding agents group, the fake first on PATH
            for c in (1, 2):
                await pg.mouse("mousePressed", 300, 300, c)
                await pg.mouse("mouseReleased", 300, 300, c)
            await pg.until("!!document.querySelector('.action-popup .tile')", 10)
            await pg.until(f"[...document.querySelectorAll('.action-popup .tile')].some(e=>(e.querySelector('.lb')||{{}}).textContent==='{FAKE}')", 10)
            hint = await pg.js(f"(()=>{{const t=[...document.querySelectorAll('.action-popup .tile[data-section=coding]')].find(e=>(e.querySelector('.lb')||{{}}).textContent==='{FAKE}'); return t ? t.title : null}})()")
            # listed by its name alone — no path under or on the tile (owner 2026-10-09)
            r["coding_listed"] = hint == FAKE
            await pg.cdp("Input.dispatchKeyEvent", type="keyDown", key="Escape", code="Escape", windowsVirtualKeyCode=27)
            await asyncio.sleep(0.5)
            # c) its tile starts exactly that program, interactive
            r["coding_clicked"] = await tile(pg, 300, 300, FAKE)
            r["coding_runs"] = bool(await pg.until(f"{frames}.some(f=>(f=>{{const t=f.querySelector('.frame-body')?.xterm; if(!t) return ''; const b=t.buffer.active; let s=''; for(let i=0;i<b.length;i++){{const l=b.getLine(i); if(l) s+=l.translateToString(true)+'\\n';}} return s;}})(f).includes('{MARK}'))", secs))
            await pg.js("document.querySelector('.agent-frame .xterm-helper-textarea')?.focus()")
            await pg.cdp("Input.insertText", text="ping-61")
            await pg.cdp("Input.dispatchKeyEvent", type="keyDown", key="Enter", code="Enter", windowsVirtualKeyCode=13, text="\r")
            r["coding_echoes"] = bool(await pg.until(f"{frames}.some(f=>(f=>{{const t=f.querySelector('.frame-body')?.xterm; if(!t) return ''; const b=t.buffer.active; let s=''; for(let i=0;i<b.length;i++){{const l=b.getLine(i); if(l) s+=l.translateToString(true)+'\\n';}} return s;}})(f).split('ping-61').length > 2)", 10))
            cell = await pg.js(f"{frames}.find(f=>(f=>{{const t=f.querySelector('.frame-body')?.xterm; if(!t) return ''; const b=t.buffer.active; let s=''; for(let i=0;i<b.length;i++){{const l=b.getLine(i); if(l) s+=l.translateToString(true)+'\\n';}} return s;}})(f).includes('{MARK}'))?.dataset.id")
            rec = next((c for c in rig.cells() if c.get("id") == cell), {})
            r["coding_cell"] = rec.get("command") == [str(fake)]
            backend = rec.get("backend_id", "")
            # the exact line: ONE pattern, the shell-joined argv (a Windows path is
            # quoted) with every character literal — unescaped, it is that line
            # (the escaper is rust's, which leaves `-` bare where python's does not)
            line = shlex.join([str(fake)])
            pats = (rig.host_record(backend) or {}).get("allowed") or []
            literal = len(pats) == 1 and not set(re.sub(r"\\.", "", pats[0])) & set(".^$*+?{}[]|()")
            r["coding_exact"] = literal and re.sub(r"\\(.)", r"\1", pats[0]) == line
            # d) a file tree cell from the double-click: the backend is a folder in it
            tree = await open_tree(pg, 1250, 80)
            r["tree_cell"] = tree is not None
            guid1 = rig.page_guid()
            r["tree_lists_backend"] = bool(tree) and await reveal(guid1, tree, backend) and bool(await pg.until(
                f"!!document.querySelector('.files-view .ft-row.agent[data-agent={backend!r}]')", 10))
            info["main_backend"] = backend
            # e) a peer canvas — the canvas node's `new_canvas`, opened by its URL
            from canvas import host_call
            made = await host_call(rig.port, "canvas_root", {"type": "new_canvas", "name": "second"})
            info["new_canvas"] = made
            await pg.open(rig.url + f"?canvas={made.get('id', '')}")
            r["peer_opened"] = bool(await pg.until(f"location.search.includes('canvas={PEER}')", secs))
            await pg.until("!!document.getElementById('canvas')", 20)
            await asyncio.sleep(2)
            r["peer_empty"] = await pg.js(f"{frames}.length === 0")
            tree2 = await open_tree(pg, 300, 200)
            peer_yaml = rig.proj / f".fantastic/web/{PEER}/agents/canvas/agent.yaml"
            guid2 = (yaml.safe_load(peer_yaml.read_text()) or {}).get("guid") if peer_yaml.exists() else None
            # the whole project shows (a superset); the first canvas's agents are on another canvas
            shown = bool(tree2 and guid2) and await reveal(guid2, tree2, backend) and await reveal(guid2, tree2, PEER)
            row = f"document.querySelector('.files-view .ft-row.agent[data-agent={backend!r}]')"
            # marked as another canvas's, and not a live row: no ↗ to open it
            elsewhere = shown and bool(await pg.until(f"!!{row}?.classList.contains('elsewhere') && !{row}.querySelector('.go')", 10))
            info["peer_tree"] = await pg.js("[...document.querySelectorAll('.files-view .ft-row.agent')].map(e=>e.dataset.agent+(e.classList.contains('elsewhere')?' (elsewhere)':''))")
            r["peer_marks_elsewhere"] = elsewhere

    info: dict = {}
    info["headless_canvas"] = (both.stderr or both.stdout).strip()[-200:]
    try:
        asyncio.run(first())
        main_rec = rig.proj / ".fantastic/web/agents/canvas/agent.yaml"
        peer_rec = rig.proj / f".fantastic/web/{PEER}/agents/canvas/agent.yaml"
        g1 = (yaml.safe_load(main_rec.read_text()) or {}).get("guid") if main_rec.exists() else None
        g2 = (yaml.safe_load(peer_rec.read_text()) or {}).get("guid") if peer_rec.exists() else None
        r["peer_own_records"] = bool(g1 and g2 and g1 != g2)
        # f) a page bundle in the peer's own folder, an instance seeded, edited live
        rig.stop()
        folder = rig.proj / f".fantastic/web/{PEER}/bundles/card"
        folder.mkdir(parents=True, exist_ok=True)
        (folder / "bundle.yaml").write_text("base: html_agent\nsentence: A card.\nhtml: |\n  <p id=v>card-one</p>\n")
        inst = rig.proj / f".fantastic/web/{PEER}/agents/canvas/agents/view_root/agents/card_1"
        inst.mkdir(parents=True, exist_ok=True)
        (inst / "agent.yaml").write_text(yaml.safe_dump({"id": "card_1", "parent_id": "view_root", "handler_module": "card"}))
        rig.start()

        async def second() -> None:
            async with rig.page(1600, 1000) as pg:
                await pg.open(rig.url + f"?canvas={PEER}")
                await pg.until("!!document.getElementById('canvas')", 20)

                async def body_has(text: str) -> bool:
                    for _ in range(secs * 2):
                        if any(isinstance(v, str) and text in v for v in await frame_eval(pg, "document.body ? document.body.innerText : ''")):
                            return True
                        await asyncio.sleep(0.5)
                    return False

                r["bundle_runs"] = await body_has("card-one")
                (folder / "bundle.yaml").write_text("base: html_agent\nsentence: A card.\nhtml: |\n  <p id=v>card-two</p>\n")
                r["bundle_reloads"] = await body_has("card-two")

        asyncio.run(second())
    finally:
        rig.stop()
    ok = all(r.values())
    rig.summary["checks"] = r
    rig.summary["info"] = info
    failed = [k for k, v in r.items() if not v]
    return verdict(*rig.finish(ok, "all checks pass" if ok else f"failed: {', '.join(failed)}"))


if __name__ == "__main__":
    sys.exit(0 if main() else 1)
