"""E89 file_tree — a live file tree, a previewer and an editor (#106). Three cells
over the project's own folder: a `file tree`, a preview PINNED to `src/one.txt`,
and a preview that FOLLOWS the tree.

a) the tree lists the project; opening `src` lists it;
b) a file made on disk appears in the open folder, without a reload;
c) a file changed on disk updates the pinned preview;
d) a file removed on disk leaves the tree;
e) an AI-style `select` (`<guid>|<tree>`) moves the selection, and the follower
   renders the Markdown it selects;
f) an agent's folder carries its badge; the follower shows its `agent.yaml`, and
   its ⇄ chip switches it to the live agent;
g) edit and save (⌘S / Ctrl-S): the disk has the new text, another preview of the
   file follows it;
h) the file changed on disk under unsaved edits: the conflict is shown, NOTHING is
   overwritten, `save` is refused as a conflict, and Reload takes the disk's text;
   Compare shows `.pv-diff`; Keep mine overwrites the disk with the editor's text;
i) the filter narrows the loaded tree, and ↓ from it goes into the tree;
j) collapsing `src` drops its watch from the bridge;
k) once the cells close, the file bridge holds no watch of theirs; no page error."""

from __future__ import annotations

import asyncio
import json
import sys
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[2]))
from _lib.canvas_live import Rig, verdict  # noqa: E402

FILE_VERBS = ["list", "read", "read_stream", "watch_path", "unwatch_path", "write", "edit", "undo", "watch"]
TREE, PIN, FOLLOW = "tree_e89", "prev_pin", "prev_follow"
LEAKS = ["", ".", "src", "docs", "src/one.txt", "docs/a.md"]


def cell(cid: str, handler: str, x: int, y: int, w: int, h: int, **extra) -> dict:
    return {"id": cid, "handler_module": handler, "display_name": cid, "can_call": {"files": FILE_VERBS}, "x": x, "y": y, "width": w, "height": h, **extra}


def frame(cid: str) -> str:
    return f"document.querySelector('.agent-frame[data-id=\"{cid}\"]')"


def main() -> bool:
    rig = Rig(__file__, live=False)
    rig.build()
    rig.make_home(None)
    r: dict = {}
    info: dict = {}
    src = rig.proj / "src"
    docs = rig.proj / "docs"
    src.mkdir()
    docs.mkdir()
    (src / "one.txt").write_text("one")
    (docs / "a.md").write_text("# Title\n\nsome *text*\n")
    rig.init(cells=[
        cell(TREE, "files_view.ts", 40, 60, 320, 600),
        cell(PIN, "preview_view.ts", 600, 60, 520, 360, pinned="src/one.txt", follow=False),
        cell(FOLLOW, "preview_view.ts", 600, 460, 520, 420, follow=True),
    ])

    async def run() -> None:
        from canvas import host_call

        guid = rig.page_guid()
        call = lambda cid, payload: host_call(rig.port, f"{guid}|{cid}", payload)  # noqa: E731
        async with rig.page(1400, 920) as pg:
            code = lambda cid: f"{frame(cid)}?.querySelector('.pv-code')?.textContent || ''"  # noqa: E731
            row = lambda path: f"{frame(TREE)}?.querySelector('.ft-row[data-path=\"{path}\"]')"  # noqa: E731
            mod = 4 if await pg.js("/Mac|iPhone|iPad/.test(navigator.platform)") else 2

            async def disk_is(p: Path, text: str, secs: float = 10) -> bool:
                end = time.time() + secs
                while time.time() < end:
                    if p.exists() and p.read_text() == text:
                        return True
                    await asyncio.sleep(0.1)
                return False

            async def held_paths() -> list:
                """The paths the bridge holds, read through an `unwatch_path` of a path nobody holds."""
                u = await host_call(rig.port, "files", {"type": "unwatch_path", "path": "e89_sentinel"})
                return u.get("paths") or []

            async def type_at_end(cid: str, text: str) -> None:
                await pg.js(f"(()=>{{const t={frame(cid)}.querySelector('.pv-input'); t.focus(); t.setSelectionRange(t.value.length, t.value.length);}})()")
                await pg.cdp("Input.insertText", text=text)

            # 1 the tree opens: the project's rows
            r["1 the tree lists the project (src, docs, .fantastic)"] = bool(await pg.until(f"!!{row('src')} && !!{row('docs')} && !!{row('.fantastic')}", 30))
            r["1b .fantastic is highlighted"] = bool(await pg.js(f"{row('.fantastic')}?.classList.contains('fantastic')"))
            # 2 expand src by its triangle
            await pg.js(f"{row('src')}.querySelector('.tw').click()")
            r["2 opening src lists it"] = bool(await pg.until(f"!!{row('src/one.txt')}", 15))
            # 3 a file made on disk appears, without a reload
            (src / "new.txt").write_text("n")
            r["3 a created file shows without a reload"] = bool(await pg.until(f"!!{row('src/new.txt')}", 15))
            # 4 the pinned preview follows a change on disk
            r["4a the pinned preview shows its file"] = bool(await pg.until(f"({code(PIN)}).includes('one')", 15))
            (src / "one.txt").write_text("two")
            r["4 a modified file updates the pinned preview"] = bool(await pg.until(f"({code(PIN)}).includes('two')", 15))
            # 5 removed on disk, gone from the tree
            (src / "new.txt").unlink()
            r["5 a deleted file leaves the tree"] = bool(await pg.until(f"!{row('src/new.txt')}", 15))
            # 6 an AI-style select
            a = await call(TREE, {"type": "select", "path": "docs/a.md"})
            info["6_select"] = a
            r["6a select replies selected"] = a.get("selected") == "docs/a.md" and a.get("kind") == "file"
            r["6b the row is the selection"] = bool(await pg.until(f"!!{frame(TREE)}?.querySelector('.ft-row.sel[data-path=\"docs/a.md\"]')", 10))
            r["6c the follower renders the Markdown"] = bool(await pg.until(f"{frame(FOLLOW)}?.querySelector('.pv-md h1')?.textContent === 'Title'", 15))
            # 7 an agent folder: badge, agent.yaml, then the agent
            sel: dict = {}
            end = time.time() + 30  # the agent folders are read from disk a moment after the tree mounts
            while time.time() < end:
                sel = await call(TREE, {"type": "select", "agent": "files"})
                if "selected" in sel:
                    break
                await asyncio.sleep(0.5)
            info["7_select_agent"] = sel
            r["7a select by agent id finds its folder"] = bool(sel.get("selected", "").endswith("/files")) and sel.get("agent") == "files"
            r["7b the folder row carries a badge"] = bool(await pg.until(f"({frame(TREE)}?.querySelector('.ft-row.agent[data-agent=\"files\"] .gl')?.textContent || '').trim().length > 0", 10))
            r["7c the follower shows the agent's agent.yaml"] = bool(await pg.until(f"({code(FOLLOW)}).includes('file_bridge')", 15))
            await pg.js(f"[...{frame(FOLLOW)}.querySelectorAll('.pv-chip')].find(b=>b.textContent.includes('⇄'))?.click()")
            r["7d the ⇄ chip shows the live agent"] = bool(await pg.until(f"({frame(FOLLOW)}?.querySelector('.reflect-view')?.textContent || '').includes('files')", 20))
            # 8 edit, type, save
            e = await call(PIN, {"type": "edit"})
            info["8_edit"] = e
            r["8a edit replies editing"] = e.get("editing") == "src/one.txt"
            r["8b the editor opens"] = bool(await pg.until(f"!!{frame(PIN)}?.querySelector('.pv-input')", 10))
            await type_at_end(PIN, " three")
            r["8c unsaved edits are marked"] = bool(await pg.until(f"!!{frame(PIN)}?.querySelector('.pv-unsaved')", 10))
            await pg.cdp("Input.dispatchKeyEvent", type="keyDown", key="s", code="KeyS", windowsVirtualKeyCode=83, modifiers=mod)
            await pg.cdp("Input.dispatchKeyEvent", type="keyUp", key="s", code="KeyS", windowsVirtualKeyCode=83, modifiers=mod)
            r["8 the save reaches the disk"] = await disk_is(src / "one.txt", "two three")
            s = await call(TREE, {"type": "select", "path": "src/one.txt"})
            info["8_select"] = s
            r["8d another preview of the file shows it"] = bool(await pg.until(f"({code(FOLLOW)}).includes('two three')", 15))
            # 9 a change on disk under unsaved edits: loud, nothing overwritten
            await type_at_end(PIN, " four")
            r["9a unsaved again"] = bool(await pg.until(f"!!{frame(PIN)}?.querySelector('.pv-unsaved')", 10))
            (src / "one.txt").write_text("disk wins")
            r["9b the conflict is shown"] = bool(await pg.until(f"{frame(PIN)}?.querySelector('.pv-conflict') && !{frame(PIN)}.querySelector('.pv-conflict').hidden", 15))
            r["9c nothing was overwritten"] = (src / "one.txt").read_text() == "disk wins"
            sv = await call(PIN, {"type": "save"})
            info["9_save"] = sv
            r["9d save is refused as a conflict"] = sv.get("reason") == "conflict"
            r["9e still nothing overwritten"] = (src / "one.txt").read_text() == "disk wins"
            await pg.js(f"[...{frame(PIN)}.querySelectorAll('.pv-conflict .pv-chip')].find(b=>b.textContent==='Reload')?.click()")
            r["9 Reload takes the disk's text and clears the banner"] = bool(await pg.until(
                f"{frame(PIN)}?.querySelector('.pv-input')?.value === 'disk wins' && {frame(PIN)}.querySelector('.pv-conflict').hidden", 15))
            # 9f Compare, 9g Keep mine
            await type_at_end(PIN, " five")
            r["9f unsaved before the next conflict"] = bool(await pg.until(f"!!{frame(PIN)}?.querySelector('.pv-unsaved')", 10))
            (src / "one.txt").write_text("disk two")
            r["9g the conflict is shown again"] = bool(await pg.until(f"{frame(PIN)}?.querySelector('.pv-conflict') && !{frame(PIN)}.querySelector('.pv-conflict').hidden", 15))
            await pg.js(f"[...{frame(PIN)}.querySelectorAll('.pv-conflict .pv-chip')].find(b=>b.textContent==='Compare')?.click()")
            r["9h Compare shows mine and the disk's side by side"] = bool(await pg.until(f"!!{frame(PIN)}?.querySelector('.pv-diff')", 15))
            r["9i Compare overwrote nothing"] = (src / "one.txt").read_text() == "disk two"
            await pg.js(f"[...{frame(PIN)}.querySelectorAll('.pv-conflict .pv-chip')].find(b=>b.textContent==='Keep mine')?.click()")
            r["9j Keep mine overwrites the disk with the editor's text"] = await disk_is(src / "one.txt", "disk wins five")
            # i) the filter and the keyboard
            await pg.js(f"(()=>{{const f={frame(TREE)}.querySelector('.ft-filter'); f.value='one.txt'; f.dispatchEvent(new Event('input',{{bubbles:true}}));}})()")
            r["i1 the filter narrows the loaded tree"] = bool(await pg.until(f"!!{row('src/one.txt')} && !{row('.fantastic')}", 10))
            await pg.js(f"{frame(TREE)}.querySelector('.ft-filter').dispatchEvent(new KeyboardEvent('keydown',{{key:'ArrowDown',bubbles:true}}))")
            r["i2 ArrowDown from the filter goes into the tree"] = bool(await pg.until(f"document.activeElement === {frame(TREE)}.querySelector('.ft-tree')", 10))
            await pg.js(f"{frame(TREE)}.querySelector('.ft-tree').dispatchEvent(new KeyboardEvent('keydown',{{key:'ArrowDown',bubbles:true}}))")
            r["i3 an arrow key moves the cursor row"] = bool(await pg.until(f"!!{frame(TREE)}.querySelector('.ft-row.cur')", 10))
            await pg.js(f"(()=>{{const f={frame(TREE)}.querySelector('.ft-filter'); f.value=''; f.dispatchEvent(new Event('input',{{bubbles:true}}));}})()")
            r["i4 clearing the filter shows the tree again"] = bool(await pg.until(f"!!{row('.fantastic')}", 10))
            # j) collapsing src drops its watch
            r["j1 src is held while open"] = "src" in await held_paths()
            await pg.js(f"{row('src')}.querySelector('.tw').click()")
            gone_src = False
            end = time.time() + 10  # the unwatch is asynchronous
            while time.time() < end:
                if "src" not in await held_paths():
                    gone_src = True
                    break
                await asyncio.sleep(0.25)
            r["j2 collapsing src unwatches it"] = gone_src
            # 10 close the cells: no watch of theirs is left on the bridge
            for cid in (TREE, PIN, FOLLOW):
                await call("view_root", {"type": "remove", "id": cid})
            r["10a the frames are gone"] = bool(await pg.until(f"!{frame(TREE)} && !{frame(PIN)} && !{frame(FOLLOW)}", 15))
            held: list = []
            end = time.time() + 10  # unmount runs asynchronously
            while time.time() < end:
                held = await held_paths()
                if not set(LEAKS) & set(held):
                    break
                await asyncio.sleep(0.25)
            info["10_paths"] = held
            r["10 no watch of the cells' is left"] = not set(LEAKS) & set(held)
            info["page_errors"] = [x for x in pg.logs if "WebGL" not in x][:8]
            r["11 no page errors"] = not info["page_errors"]

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
