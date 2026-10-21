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

Then the canvas's own sidebar (#148), the file tree fixed beside the canvas:
h) zoomed far out, the sidebar stays where it was; both cells are hidden again,
   the two agent rows marked IN THE SIDEBAR are dragged together and both cells
   come back at the drop point, and a file row dragged from it opens a preview at
   its drop point;
i) the sidebar is collapsed, the page is reloaded, and it is still collapsed.

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


SIDE = "document.querySelector('.tree-sidebar')"
# the zoom of the world, read off its transform (the canvas writes `scale(z)`)
SCALE = "parseFloat((/scale\\(([\\d.]+)\\)/.exec(document.querySelector('.canvas-world').style.transform)||[0,'1'])[1])"


def side_row(sel: str) -> str:
    """A row of the canvas's own sidebar, by its CSS selector (#148)."""
    return f"document.querySelector('.tree-sidebar .ft-tree')?.querySelector('{sel}')"


def placed(expr: str, x: int, y: int, dwx: int = -260, dwy: int = -280) -> str:
    """How far the element `expr` is from where a drop at screen (x, y) puts its corner, as [dx, dy] (or null).
    A drop centres its spots on the point (`layoutAround`): one spot is the canvas's drop slot (520 x 560
    world units, `canvas.ts` DROP_SLOT), so its corner is the point less half a slot (dwx, dwy: the corner's
    offset from the point, in world units, at the zoom in use)."""
    return (
        f"(()=>{{const e={expr}; if(!e) return null; const z={SCALE}; const b=e.getBoundingClientRect();"
        f" return [b.left-({x}+({dwx})*z), b.top-({y}+({dwy})*z)];}})()"
    )


def geom(expr: str) -> str:
    """Where an element sits, three ways: its world rect (the style's left/top, in world units), the view's transform, and its screen rect with the canvas's."""
    return (
        f"(()=>{{const e={expr}; if(!e) return null; const w=document.querySelector('.canvas-world'); const c=document.getElementById('canvas');"
        " const b=e.getBoundingClientRect(); const cb=c.getBoundingClientRect();"
        " return {left: e.style.left, top: e.style.top, transform: w.style.transform, screen: [b.left, b.top, b.width, b.height], canvas: [cb.left, cb.top, cb.width, cb.height]};})()"
    )


def pair(a: str, b: str, x: int, y: int) -> str:
    """For two cells, the offset of each from its nearer spot of the two a drop of two items plans (`layoutAround`:
    the grid is 1064 world units wide, the spots 544 apart, the first 532 units left of the point), and which spot it is."""
    return (
        f"(()=>{{const z={SCALE}; const sp=[[{x}-532*z,{y}-280*z],[{x}+12*z,{y}-280*z]]; const out=[];"
        f" for (const e of [{a},{b}]) {{ if(!e) return null; const r=e.getBoundingClientRect(); let best=null;"
        " sp.forEach(([sx,sy],i)=>{const d=[r.left-sx,r.top-sy]; const m=Math.max(Math.abs(d[0]),Math.abs(d[1])); if(best===null||m<best.m) best={m,i,d};});"
        " out.push({spot: best.i, dx: best.d[0], dy: best.d[1]}); } return out;})()"
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
    reveal_log: list = []
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

            async def reveal(cell: str, agent: str, secs_: float = 10) -> bool:
                """Show `agent`'s folder in the tree cell `cell` through its `reveal` verb, the way E61 does:
                a first `not_found` (the bridge's `list` is still walking the disk) is asked again for
                at most `secs_`; any other refusal ends it."""
                from canvas import host_call

                end = asyncio.get_event_loop().time() + secs_
                while asyncio.get_event_loop().time() < end:
                    a = await host_call(rig.port, f"{rig.page_guid()}|{cell}", {"type": "reveal", "agent": agent})
                    reveal_log.append({"agent": agent, "reply": a})
                    if "revealed" in a:
                        return True
                    if a.get("reason") != "not_found":
                        return False
                    await asyncio.sleep(0.5)
                return False

            async def side_reveal(what: dict, secs_: float = 10) -> bool:
                """Show a folder (or an agent's) in the canvas's sidebar, through the root view's `tree` verb."""
                end = asyncio.get_event_loop().time() + secs_
                while asyncio.get_event_loop().time() < end:
                    a = await view({"type": "tree", "verb": "reveal", **what})
                    reveal_log.append({"sidebar": what, "reply": a})
                    if "revealed" in a:
                        return True
                    if a.get("reason") != "not_found":
                        return False
                    await asyncio.sleep(0.5)
                return False

            async def last_tick(cid: str) -> int:
                text = await pg.js(f"({term_text(cid)})")
                m = TICK.findall(text or "")
                return int(m[-1]) if m else 0

            async def frames() -> int:
                return int(await pg.js("document.querySelectorAll('.agent-frame').length") or 0)

            # a) the tree lists both agents; both cells draw
            # the agent folders are collapsed until revealed: each is shown through the tree cell's own verb
            r["a1 the tree lists both agents"] = bool(await reveal(TREE, BACKEND_A) and await reveal(TREE, BACKEND_B)) and bool(await pg.until(f"!!{agent_row(BACKEND_A)} && !!{agent_row(BACKEND_B)}", 30))
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
            # h) the canvas's own sidebar (#148): zoomed far out, it stays put; its rows drag onto the canvas
            await pg.js(
                "(()=>{const c=document.getElementById('canvas'); for(let i=0;i<30;i++) c.dispatchEvent(new WheelEvent('wheel',{deltaY:100,bubbles:true,cancelable:true,clientX:760,clientY:420}));})()"
            )
            r["h1 zoomed far out, the sidebar stays put"] = bool(await pg.until(f"{SCALE} < 0.3 && {SIDE}?.getBoundingClientRect().left === 0", 20))
            # beside the canvas, not over it: the canvas starts where the sidebar ends (a wide screen's margin)
            r["h1b the canvas starts where the sidebar ends, and covers none of it"] = bool(await pg.until(
                f"Math.abs(document.getElementById('canvas').getBoundingClientRect().left - {SIDE}.getBoundingClientRect().right) < 1 && {SIDE}.getBoundingClientRect().right > 0", 10))
            agent_sel = f'.ft-row[data-agent="{BACKEND_A}"]'
            agent_sel_b = f'.ft-row[data-agent="{BACKEND_B}"]'
            file_sel = '.ft-row[data-path="src/one.txt"]'
            r["h2 the sidebar lists both agent folders"] = bool(
                await side_reveal({"agent": BACKEND_A}) and await side_reveal({"agent": BACKEND_B})
                and await pg.until(f"!!{side_row(agent_sel)} && !!{side_row(agent_sel_b)}", 15))
            await view({"type": "hide", "id": CELL_A})
            await view({"type": "hide", "id": CELL_B})
            r["h3 both cells are hidden again"] = bool(await pg.until(f"!{frame(CELL_A)} && !{frame(CELL_B)}", 15))
            # two hidden agents, marked IN THE SIDEBAR (a click, then ⌘/Ctrl-click), dragged together
            await pg.js(f"{side_row(agent_sel)}.click()")
            await pg.js(click_mod(side_row(agent_sel_b)))
            r["h4a both sidebar rows are marked"] = bool(await pg.until(
                f"!!{side_row(agent_sel)}?.classList.contains('marked') && !!{side_row(agent_sel_b)}?.classList.contains('marked')", 10))
            info["h_drop"] = await pg.js(drag_drop(side_row(agent_sel), 760, 420))
            r["h4 both agents dragged from the sidebar come back"] = info["h_drop"] is True and bool(await pg.until(f"!!{frame(CELL_A)} && !!{frame(CELL_B)}", 20))
            info["h_pair"] = await pg.js(pair(frame(CELL_A), frame(CELL_B), 760, 420))
            # both sit on the two spots of the drop, one each. The bound is what the geometry leaves: a spot's corner
            # is a whole world unit (at most half a unit, 0.5 * zoom, off the point) and the view's offset is a
            # whole pixel (at most half a pixel off), plus a pixel of sub-pixel rounding in the screen rect.
            tol = await pg.js(f"0.5*{SCALE}+1")
            info["h_tol"] = tol
            pr = info["h_pair"] or [{"spot": -1, "dx": 999, "dy": 999}, {"spot": -1, "dx": 999, "dy": 999}]
            r["h5 the pair lands at the drop point, one spot each, zoomed far out"] = (
                pr[0]["spot"] != pr[1]["spot"] and all(abs(c["dx"]) <= tol and abs(c["dy"]) <= tol for c in pr))
            info["h_agent_geom"] = await pg.js(geom(frame(CELL_A)))
            r["h6 the other cell stays where the drop put it"] = bool(await pg.js(f"!!{frame(CELL_B)}"))
            before = set(await pg.js("[...document.querySelectorAll('.agent-frame[data-id^=\"preview_\"]')].map(f=>f.dataset.id)") or [])
            await side_reveal({"path": "src/one.txt"})
            r["h7 the file shows in the sidebar"] = bool(await pg.until(f"!!{side_row(file_sel)}", 15))
            # the file goes to an empty spot of the world (a cell already there would push the new one aside)
            fx, fy = await pg.js(
                "(()=>{const m=/translate\\(([-\\d.]+)px, ([-\\d.]+)px\\) scale\\(([\\d.]+)\\)/.exec(document.querySelector('.canvas-world').style.transform); const z=+m[3]; return [Math.round(+m[1]+z*2400), Math.round(+m[2]+z*1400)];})()"
            )
            info["h_file_at"] = [fx, fy]
            info["h_file"] = await pg.js(drag_drop(side_row(file_sel), fx, fy))
            fresh = "[...document.querySelectorAll('.agent-frame[data-id^=\"preview_\"]')].map(f=>f.dataset.id).filter(i=>!%s.includes(i))" % json.dumps(sorted(before))
            r["h8 a file dragged from the sidebar opens a preview"] = bool(await pg.until(f"({fresh}).length === 1", 20))
            info["h_file_geom"] = await pg.js(geom(f"document.querySelector('.agent-frame[data-id=\"'+({fresh})[0]+'\"]')"))
            c_file = await pg.js(placed(f"document.querySelector('.agent-frame[data-id=\"'+({fresh})[0]+'\"]')", fx, fy))
            info["h_file_corner_off"] = c_file
            r["h9 the preview lands at the drop point, zoomed far out"] = bool(c_file) and abs(c_file[0]) <= tol and abs(c_file[1]) <= tol

            # g) the cells this probe made are removed; no page error
            made = await pg.js("[...document.querySelectorAll('.agent-frame[data-id^=\"preview_\"], .agent-frame[data-id^=\"files_\"]')].map(f=>f.dataset.id)")
            for cid in [TREE, CELL_A, CELL_B, *made]:
                await view({"type": "remove", "id": cid})
            r["g1 the cells are gone"] = bool(await pg.until(f"!{frame(TREE)} && !{frame(CELL_A)} && !{frame(CELL_B)}", 15))
            info["page_errors"] = [x for x in pg.logs if "WebGL" not in x][:8]
            r["g2 no page errors"] = not info["page_errors"]
            # i) collapsed, the choice is kept on the canvas: a reload brings the sidebar back collapsed
            await pg.js("document.querySelector('.tree-sidebar .ts-toggle').click()")
            r["i1 collapsed, the sidebar is a rail"] = bool(await pg.until(f"{SIDE}?.classList.contains('collapsed')", 10))
            # the choice reaches the canvas's record on disk before the page goes: the loader persists on its debounce
            saved = False
            end = asyncio.get_event_loop().time() + 15
            while asyncio.get_event_loop().time() < end and not saved:
                saved = (rig.host_record("view_root") or {}).get("tree_sidebar", {}).get("open") is False
                await asyncio.sleep(0.25)
            r["i1a the collapse is saved on the canvas's record"] = saved
            await pg.open(rig.url)
            await pg.until("!!document.getElementById('canvas')", 30)
            r["i2 after a reload it is still collapsed"] = bool(await pg.until(f"{SIDE}?.classList.contains('collapsed')", 20))

    try:
        asyncio.run(run())
    finally:
        rig.stop()
    rig.summary["checks"] = r
    info["reveal_log"] = reveal_log
    rig.summary["info"] = info
    print(json.dumps(info, indent=1, default=str)[:3000])
    failed = [k for k, v in r.items() if not v]
    return verdict(*rig.finish(not failed, f"{len(r) - len(failed)}/{len(r)} pass" + ("" if not failed else f" — failed: {'; '.join(failed)}")))


if __name__ == "__main__":
    sys.exit(0 if main() else 1)
