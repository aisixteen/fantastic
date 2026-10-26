"""E128 import_drop — a blueprint folder dropped on the canvas (#269).

A file-tree cell, a host agent shown by a terminal cell, and project folders `bp` (3 agents),
`bpport` (one port) and `bpcode` (a code file). Every drop is a DataTransfer dispatched on the
canvas or on a cell's frame (CDP drags are not reliable), as E90 does.

a) the tree lists `bp`;
b) `bp` dropped on the canvas: the `import blueprint` dialog holds `would create 3 agents`;
   nothing is created, and the host saw the preview only;
c) Cancel: nothing created, no further call;
d) dropped again and approved: the three agents exist under the canvas host and a toast says
   `imported` with their ids;
e) dropped on the cell: the new agents' parent is the cell's backend;
f) `bpport` (a port): the `import: bind ports` dialog has one input; the agent typed and
   Preview pressed, the preview shows it; Approve imports, bound;
g) `bpcode`: a toast with `allow_code`, no dialog, nothing created;
h) no page error."""

from __future__ import annotations

import asyncio
import json
import os
import re
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[2]))
from _lib.blueprint import PLAIN, STORE, TEAM, agent_ids, record, send, write_blueprint  # noqa: E402
from _lib.canvas_live import FOLDER, MAC_CHROME, Rig, py_job, verdict  # noqa: E402

TICKER = "import time\nwhile True:\n    print('TICK', flush=True)\n    time.sleep(0.5)\n"
TREE = "tree_e128"
CELL, BACKEND = "cell_e128", "exec_bridge_e128"
VERBS = ["watch", "status", "start", "write", "ack", "resize", "output"]
FILE_VERBS = ["list", "read", "read_stream", "watch_path", "unwatch_path", "write", "edit", "undo", "watch"]
CANVAS_HOST = "canvas_root"
POPUP = ".action-popup"


def frame(cid: str) -> str:
    return f"document.querySelector('.agent-frame[data-id=\"{cid}\"]')"


def drop(src_expr: str, target_expr: str, x: int, y: int) -> str:
    """Dispatch a tree drag: dragstart on the source row, then dragover and drop on `target_expr` (the canvas, or a cell's frame) at (x, y)."""
    return (
        f"(()=>{{const src={src_expr}; const t={target_expr}; if(!src||!t) return 'no source or target';"
        " const dt=new DataTransfer();"
        " src.dispatchEvent(new DragEvent('dragstart',{bubbles:true,cancelable:true,dataTransfer:dt}));"
        " const o={bubbles:true,cancelable:true,dataTransfer:dt,clientX:%d,clientY:%d};"
        " t.dispatchEvent(new DragEvent('dragover',o)); t.dispatchEvent(new DragEvent('drop',o));"
        " return dt.getData('application/x-fantastic-tree').length>0;})()" % (x, y)
    )


def button(label: str) -> str:
    return f"[...document.querySelectorAll('{POPUP} button')].find(b=>b.textContent==='{label}')"


def main() -> bool:
    chrome = os.environ.get("CHROME") or MAC_CHROME
    if not Path(chrome).exists():
        print(f"E2E-SKIP: no Chrome/CDP on this machine ({chrome} is missing; set CHROME)")
        return True
    rig = Rig(__file__, live=False)
    rig.build()
    rig.make_home(None)
    rig.proj.mkdir(parents=True, exist_ok=True)
    (rig.proj / "ticker.py").write_text(TICKER)
    write_blueprint(rig, "bp", PLAIN)
    write_blueprint(rig, "bpport", TEAM)
    write_blueprint(rig, "bpcode", "agents:\n  - id: x\n    handler_module: ram_store.tools\n", {"x/tools.py": "def hello():\n    return 1\n"})
    tree = {"id": TREE, "handler_module": "files_view.ts", "display_name": "file tree", "can_call": {"files": FILE_VERBS},
            "x": 40, "y": 60, "width": 320, "height": 640}
    cell = {"id": CELL, "handler_module": "terminal_view.ts", "backend_id": BACKEND, "owns_backend": False,
            "command": py_job("ticker.py")[0], "can_call": {"$self": "*", BACKEND: VERBS}, "callable_by": {"$self": "*"},
            "display_name": CELL, "x": 420, "y": 60, "width": 420, "height": 240}
    host = {"id": BACKEND, "handler_module": "exec_bridge.tools", "cwd": ".", "inherit_env": ["*"], "shell": "login",
            "ingress_rule": "allow_all_ingress", "allowed": [py_job("ticker.py")[1]], "cell_id": CELL, "can_call": {"$self": "*"},
            "callable_by": {"$self": "*", "mcp": ["*"], "user": ["*"], "canvas_root": ["*"], "canvas_ws": ["watch"]}}
    r: dict = {}
    info: dict = {}
    rig.init(cells=[tree, cell], host_agents=[(FOLDER, host)])
    send(rig, "kernel_state", {"type": "create_agent", "handler_module": "sqlite_store.tools", "id": STORE,
                               "parent_id": "kernel_state", "path": f"{STORE}.db"})

    def previews() -> int:
        """How many `import_subtree` calls the host has been sent."""
        return len(re.findall(r"call import_subtree", rig.log.read_text(errors="replace")))

    async def run() -> None:
        async with rig.page(1400, 920) as pg:
            row = lambda p: f"document.querySelector('.agent-frame[data-id=\"{TREE}\"] .ft-row[data-path=\"{p}\"]')"  # noqa: E731
            toasts = lambda: pg.js("[...window.__toasts]")  # noqa: E731
            r["a0 the tree cell is on screen"] = bool(await pg.until(f"!!{frame(TREE)} && !!{frame(CELL)}", 30))
            await pg.js("window.__toasts=[]; new MutationObserver(ms=>ms.forEach(m=>m.addedNodes.forEach(n=>{if(n.classList&&n.classList.contains('canvas-toast')) window.__toasts.push(n.textContent);}))).observe(document.body,{childList:true})")
            # a) the folder is listed
            r["a1 the tree lists bp"] = bool(await pg.until(f"!!{row('bp')}", 30))
            base = agent_ids(rig)

            # b) dropped on the canvas: the preview, nothing created
            calls0 = previews()
            info["b_drop"] = await pg.js(drop(row("bp"), "document.getElementById('canvas')", 900, 600))
            r["b1 the drop carried the tree's payload"] = info["b_drop"] is True
            r["b2 the import dialog opens"] = bool(await pg.until(f"!!document.querySelector('{POPUP} .title') && document.querySelector('{POPUP} .title').textContent==='import blueprint'", 20))
            text = await pg.js(f"(document.querySelector('{POPUP} pre')||{{}}).textContent||''")
            info["b_text"] = text
            r["b3 it says what it would create"] = "would create 3 agents" in text and "crew_1" in text and "no code" in text
            r["b4 nothing is created yet"] = agent_ids(rig) == base
            r["b5 the host saw the preview only"] = previews() == calls0 + 1
            r["b6 the folder was not opened as a tree"] = not await pg.js("[...document.querySelectorAll('.agent-frame')].some(f=>f.dataset.id.startsWith('files_'))")

            # c) Cancel
            await pg.js(f"{button('Cancel')}.click()")
            await asyncio.sleep(1.5)
            r["c1 the dialog is gone"] = not await pg.js(f"!!document.querySelector('{POPUP}')")
            r["c2 Cancel created nothing and sent nothing more"] = agent_ids(rig) == base and previews() == calls0 + 1

            # d) dropped again, approved
            await pg.js(drop(row("bp"), "document.getElementById('canvas')", 900, 600))
            await pg.until(f"!!{button('Approve')}", 20)
            await pg.js(f"{button('Approve')}.click()")
            made = None
            for _ in range(80):
                made = sorted(set(agent_ids(rig)) - set(base))
                if len(made) >= 3:
                    break
                await asyncio.sleep(0.25)
            info["d_made"] = made
            r["d1 the three agents exist after Approve"] = made == ["crew_1", "deckhand_1", "mate_1"]
            r["d2 the top one is under the canvas host"] = record(rig, "crew_1").get("parent_id") == CANVAS_HOST
            r["d3 the rest are under it"] = record(rig, "mate_1").get("parent_id") == "crew_1" and record(rig, "deckhand_1").get("parent_id") == "crew_1"
            r["d4 a toast says what was imported"] = bool(await pg.until("window.__toasts.some(t=>/^imported .*crew_1/.test(t))", 20))
            info["toasts"] = await toasts()
            r["d5 the host saw a preview and the approval"] = previews() == calls0 + 3

            # e) dropped on a cell: its backend is the parent
            base2 = agent_ids(rig)
            await pg.js(drop(row("bp"), frame(CELL), 600, 150))
            await pg.until(f"!!{button('Approve')}", 20)
            r["e0 the dialog opens on a cell drop"] = bool(await pg.js(f"!!{button('Approve')}"))
            await pg.js(f"{button('Approve')}.click()")
            made = []
            for _ in range(80):
                made = sorted(set(agent_ids(rig)) - set(base2))
                if len(made) >= 3:
                    break
                await asyncio.sleep(0.25)
            info["e_made"] = made
            r["e1 a second copy exists"] = made == ["crew_2", "deckhand_2", "mate_2"]
            r["e2 its top is under the cell's backend, not the canvas host"] = record(rig, "crew_2").get("parent_id") == BACKEND

            # f) a port: asked for, previewed, then approved
            base3 = agent_ids(rig)
            await pg.js(drop(row("bpport"), "document.getElementById('canvas')", 900, 600))
            r["f1 the bind dialog opens"] = bool(await pg.until(f"!!document.querySelector('{POPUP} .title') && document.querySelector('{POPUP} .title').textContent==='import: bind ports'", 20))
            r["f2 it asks for one agent"] = (await pg.js(f"document.querySelectorAll('{POPUP} input').length")) == 1 and \
                "@memory" in str(await pg.js(f"document.querySelector('{POPUP} input').placeholder"))
            r["f3 nothing is created while it asks"] = agent_ids(rig) == base3
            await pg.js(
                f"(()=>{{const i=document.querySelector('{POPUP} input'); i.value='{STORE}'; i.dispatchEvent(new Event('input',{{bubbles:true}}));}})()")
            await pg.js(f"{button('Preview')}.click()")
            r["f4 the preview follows, with the port bound"] = bool(await pg.until(
                f"(document.querySelector('{POPUP} .title')||{{}}).textContent==='import blueprint' && (document.querySelector('{POPUP} pre')||{{}}).textContent.includes('@memory = {STORE}')", 20))
            r["f5 still nothing is created"] = agent_ids(rig) == base3
            await pg.js(f"{button('Approve')}.click()")
            made = []
            for _ in range(80):
                made = sorted(set(agent_ids(rig)) - set(base3))
                if len(made) >= 3:
                    break
                await asyncio.sleep(0.25)
            info["f_made"] = made
            r["f6 the blueprint is imported"] = made == ["lead_1", "scout_1", "squad_1"]
            lead = record(rig, "lead_1")
            r["f7 lead is bound to the store"] = lead.get("memory_store_id") == STORE and STORE in lead.get("can_call", {})

            # g) code: refused as a toast
            base4 = agent_ids(rig)
            await pg.js(drop(row("bpcode"), "document.getElementById('canvas')", 900, 600))
            r["g1 a refusal toast names allow_code"] = bool(await pg.until("window.__toasts.some(t=>/allow_code/.test(t) && t.startsWith('✕'))", 20))
            r["g2 no dialog and nothing created"] = not await pg.js(f"!!document.querySelector('{POPUP}')") and agent_ids(rig) == base4
            info["toasts_end"] = await toasts()
            info["page_errors"] = [x for x in pg.logs if "WebGL" not in x][:8]
            r["h no page errors"] = not info["page_errors"]

    try:
        asyncio.run(run())
    finally:
        rig.stop()
    rig.summary["checks"], rig.summary["info"] = r, info
    print(json.dumps(info, indent=1, default=str)[:3000])
    failed = [k for k, v in r.items() if not v]
    return verdict(*rig.finish(not failed, f"{len(r) - len(failed)}/{len(r)} pass" + ("" if not failed else f" — failed: {'; '.join(failed)}")))


if __name__ == "__main__":
    sys.exit(0 if main() else 1)
