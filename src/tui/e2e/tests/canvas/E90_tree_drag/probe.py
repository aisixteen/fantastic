"""E90 tree_drag — drag from the file tree onto the canvas (#134, #135).

Two host agents, each shown by a terminal cell that ticks. The tree lists both
as folders; a file and a folder sit in the project.

a) the tree lists both agents; both cells draw;
b) both cells are hidden (`view_root hide`): their frames go, the rows say so;
c) the two agent rows are marked (a click, then ⌘/Ctrl-click);
d) the marked rows are dragged onto the canvas (a DataTransfer: CDP drags are not
   reliable): both cells come back where the drop was, live (their ticks advance),
   and no cell is duplicated;
e) `src/one.txt` dragged in: a preview pinned to it shows it, and follows a
   change on disk;
f) the folder `docs` dragged in: a file tree rooted at it lists `docs/a.md` and
   nothing above `docs`;
g) the cells this probe made are removed; no page error is logged.

Every page call below is `host_call` to `<page guid>|<cell>` or to `view_root`,
the way an AI reaches a cell."""

from __future__ import annotations

import asyncio
import json
import re
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[2]))
from _lib.canvas_live import FOLDER, Rig, py_job, verdict  # noqa: E402

TICKER = r'''import sys, time
n = 0
while True:
    n += 1
    print(f"TICK {n}", flush=True)
    time.sleep(0.2)
'''
TREE = "tree_e90"
CELL_A, BACKEND_A = "cell_e90a", "exec_bridge_e90a"
CELL_B, BACKEND_B = "cell_e90b", "exec_bridge_e90b"
VERBS = ["watch", "status", "start", "write", "ack", "resize", "output"]
FILE_VERBS = ["list", "read", "read_stream", "watch_path", "unwatch_path", "write", "edit", "undo", "watch"]
TICK = re.compile(r"TICK (\d+)")


def frame(cid: str) -> str:
    return f"document.querySelector('.agent-frame[data-id=\"{cid}\"]')"


def row(cid: str, sel: str) -> str:
    """A row of the tree cell `cid`, by its CSS selector."""
    return f"{frame(cid)}?.querySelector('{sel}')"


def drag_drop(src_expr: str, x: int, y: int) -> str:
    """Dispatch a tree drag: dragstart on the source row, then dragover and drop on the canvas at (x, y)."""
    return (
        f"(()=>{{const src={src_expr}; if(!src) return 'no source';"
        " const dt=new DataTransfer();"
        " src.dispatchEvent(new DragEvent('dragstart',{bubbles:true,cancelable:true,dataTransfer:dt}));"
        " const c=document.getElementById('canvas'); const o={bubbles:true,cancelable:true,dataTransfer:dt,clientX:%d,clientY:%d};"
        " c.dispatchEvent(new DragEvent('dragover',o)); c.dispatchEvent(new DragEvent('drop',o));"
        " return dt.getData('application/x-fantastic-tree').length>0;})()" % (x, y)
    )


def click_mod(src_expr: str) -> str:
    """A ⌘-click on macOS, a Ctrl-click elsewhere (the tree's own rule)."""
    return (
        f"(()=>{{const el={src_expr}; const m=/Mac|iPhone|iPad/.test(navigator.platform);"
        " el.dispatchEvent(new MouseEvent('click',{bubbles:true,cancelable:true,ctrlKey:!m,metaKey:m}));})()"
    )


def term_text(cid: str) -> str:
    """What a terminal cell's screen holds (its xterm buffer, the way the ticker probes read it)."""
    return (
        f"(()=>{{const f=document.querySelector('.agent-frame[data-id=\"{cid}\"] .frame-body'); const t=f&&f.xterm; if(!t) return '';"
        " const b=t.buffer.active; let s=''; for(let i=0;i<b.length;i++){const l=b.getLine(i); if(l) s+=l.translateToString(true)+' ';} return s;})()"
    )


def cell(cid: str, backend: str, x: int, y: int) -> dict:
    return {
        "id": cid, "handler_module": "terminal_view.ts", "backend_id": backend, "owns_backend": False,
        "command": py_job("ticker.py")[0], "can_call": {"$self": "*", backend: VERBS}, "callable_by": {"$self": "*"},
        "display_name": cid, "x": x, "y": y, "width": 420, "height": 240,
    }


def host(backend: str, cid: str) -> dict:
    return {
        "id": backend, "handler_module": "exec_bridge.tools", "cwd": ".", "inherit_env": ["*"], "shell": "login",
        "ingress_rule": "allow_all_ingress", "allowed": [py_job("ticker.py")[1]], "cell_id": cid, "can_call": {"$self": "*"},
        "callable_by": {"$self": "*", "mcp": ["*"], "user": ["*"], "canvas_root": ["*"], "canvas_ws": ["watch"]},
    }


def main() -> bool:
    rig = Rig(__file__, live=False)
    rig.build()
    rig.make_home(None)
    rig.proj.mkdir(parents=True, exist_ok=True)
    (rig.proj / "ticker.py").write_text(TICKER)
    src = rig.proj / "src"
    docs = rig.proj / "docs"
    src.mkdir()
    docs.mkdir()
    (src / "one.txt").write_text("one")
    (docs / "a.md").write_text("# Title\n")
    tree = {"id": TREE, "handler_module": "files_view.ts", "display_name": "file tree", "can_call": {"files": FILE_VERBS},
            "x": 40, "y": 60, "width": 320, "height": 640}
    r: dict = {}
    info: dict = {}
    rig.init(
        cells=[tree, cell(CELL_A, BACKEND_A, 420, 60), cell(CELL_B, BACKEND_B, 420, 340)],
        host_agents=[(FOLDER, host(BACKEND_A, CELL_A)), (FOLDER, host(BACKEND_B, CELL_B))],
    )

    async def run() -> None:
        from canvas import host_call

        call = lambda cid, payload: host_call(rig.port, f"{rig.page_guid()}|{cid}", payload)  # noqa: E731
        view = lambda payload: host_call(rig.port, f"{rig.page_guid()}|view_root", payload)  # noqa: E731
        async with rig.page(1400, 920) as pg:
            agent_row = lambda b: row(TREE, f'.ft-row[data-agent="{b}"]')  # noqa: E731
            file_row = lambda p: row(TREE, f'.ft-row[data-path="{p}"]')  # noqa: E731

            async def last_tick(cid: str) -> int:
                text = await pg.js(f"({term_text(cid)})")
                m = TICK.findall(text or "")
                return int(m[-1]) if m else 0

            async def frames() -> int:
                return int(await pg.js("document.querySelectorAll('.agent-frame').length") or 0)

            # a) the tree lists both agents; both cells draw
            r["a1 the tree lists both agents"] = bool(await pg.until(f"!!{agent_row(BACKEND_A)} && !!{agent_row(BACKEND_B)}", 30))
            r["a2 both cells draw their ticks"] = bool(await pg.until(f"/TICK \\d+/.test({term_text(CELL_A)}) && /TICK \\d+/.test({term_text(CELL_B)})", 30))
            start_frames = await frames()
            info["start_frames"] = start_frames
            # b) both cells hidden: their frames go, the rows say so
            await view({"type": "hide", "id": CELL_A})
            await view({"type": "hide", "id": CELL_B})
            r["b1 the hidden cells' frames are gone"] = bool(await pg.until(f"!{frame(CELL_A)} && !{frame(CELL_B)}", 15))
            r["b2 the rows are hidden"] = bool(await pg.until(f"!!{agent_row(BACKEND_A)}?.classList.contains('hidden') && !!{agent_row(BACKEND_B)}?.classList.contains('hidden')", 10))
            # c) mark both: a click, then a ⌘/Ctrl-click
            await pg.js(f"{agent_row(BACKEND_A)}.click()")
            await pg.js(click_mod(agent_row(BACKEND_B)))
            r["c1 both rows are marked"] = bool(await pg.until(f"!!{agent_row(BACKEND_A)}?.classList.contains('marked') && !!{agent_row(BACKEND_B)}?.classList.contains('marked')", 10))
            # d) drag the marked rows onto the canvas, at a spot of their own
            info["d_drop"] = await pg.js(drag_drop(agent_row(BACKEND_A), 760, 420))
            r["d1 the drop carried the tree's payload"] = info["d_drop"] is True
            r["d2 both cells come back"] = bool(await pg.until(f"!!{frame(CELL_A)} && !!{frame(CELL_B)}", 20))
            t_a, t_b = await last_tick(CELL_A), await last_tick(CELL_B)
            await asyncio.sleep(1.5)
            r["d3 both reattach live: their ticks advance"] = (await last_tick(CELL_A)) > t_a and (await last_tick(CELL_B)) > t_b
            r["d4 nothing is duplicated"] = (await frames()) == start_frames
            # e) a file: a preview pinned to it, following its changes
            src_tw = row(TREE, '.ft-row[data-path="src"] .tw')
            await pg.js(f"{src_tw}.click()")
            r["e1 src lists its file"] = bool(await pg.until(f"!!{file_row('src/one.txt')}", 15))
            await pg.js(drag_drop(file_row("src/one.txt"), 760, 60))
            # the new preview: the one cell that shows `one` (the drop made it; nothing else pins it)
            by_text = "[...document.querySelectorAll('.agent-frame[data-id^=\"preview_\"]')].find(f=>(f.querySelector('.pv-code')?.textContent||'').includes('one'))?.dataset.id||''"
            r["e2 the preview shows the file"] = bool(await pg.until(f"!!({by_text})", 20))
            preview = str(await pg.js(f"({by_text})"))
            info["e_preview"] = preview
            (src / "one.txt").write_text("two")
            r["e3 the preview follows a change on disk"] = bool(await pg.until(
                f"({frame(preview)}?.querySelector('.pv-code')?.textContent||'').includes('two')", 15))
            # f) a folder: a file tree rooted at it
            await pg.js(drag_drop(file_row("docs"), 1080, 60))
            by_file = "[...document.querySelectorAll('.agent-frame[data-id^=\"files_\"]')].find(f=>f.querySelector('.ft-row[data-path=\"docs/a.md\"]'))?.dataset.id||''"
            r["f1 a file tree rooted at docs lists its file"] = bool(await pg.until(f"!!({by_file})", 20))
            tree_root = str(await pg.js(f"({by_file})"))
            info["f_tree"] = tree_root
            src_row = row(tree_root, '.ft-row[data-path="src"]')
            r["f2 it shows nothing above docs"] = not await pg.js(f"!!{src_row}")
            # g) the cells this probe made are removed; no page error
            made = await pg.js("[...document.querySelectorAll('.agent-frame[data-id^=\"preview_\"], .agent-frame[data-id^=\"files_\"]')].map(f=>f.dataset.id)")
            for cid in [TREE, CELL_A, CELL_B, *made]:
                await view({"type": "remove", "id": cid})
            r["g1 the cells are gone"] = bool(await pg.until(f"!{frame(TREE)} && !{frame(CELL_A)} && !{frame(CELL_B)}", 15))
            info["page_errors"] = [x for x in pg.logs if "WebGL" not in x][:8]
            r["g2 no page errors"] = not info["page_errors"]

    try:
        asyncio.run(run())
    finally:
        rig.stop()
    rig.summary["checks"] = r
    rig.summary["info"] = info
    print(json.dumps(info, indent=1, default=str)[:3000])
    failed = [k for k, v in r.items() if not v]
    return verdict(*rig.finish(not failed, f"{len(r) - len(failed)}/{len(r)} pass" + ("" if not failed else f" — failed: {'; '.join(failed)}")))


if __name__ == "__main__":
    sys.exit(0 if main() else 1)
