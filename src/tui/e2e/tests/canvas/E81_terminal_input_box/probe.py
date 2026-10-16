"""E81 terminal_input_box — what is typed into a terminal cell reaches its
program ONCE (owner 2026-10-16: a line typed into `claude` came out repeated
from its start). xterm keeps every typed character in its hidden textarea until
Enter, Ctrl-C or blur, so an OS text service working on that box (Safari's inline
suggestions, Writing Tools) could hand the whole line back as new input:

a) a terminal cell runs REC, which logs every read of its raw stdin;
b) keys typed one by one reach REC once each, and the box is empty after them;
c) the box carries `writingsuggestions="false"` and `autocomplete="off"`;
d) an input method's composition, then its commit, reaches REC once, and the
   box is empty after it."""

from __future__ import annotations

import asyncio
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[2]))
from _lib.canvas_live import FOLDER, Rig, verdict  # noqa: E402

REC = r'''import os, sys, tty
tty.setraw(0)
sys.stdout.write("READY\r\n")
sys.stdout.flush()
while True:
    b = os.read(0, 4096)
    with open("keys.log", "ab") as f:
        f.write(b)
'''
CELL, BACKEND = "cell_rec", "exec_bridge_rec"
VERBS = ["watch", "status", "start", "write", "ack", "resize", "output"]
BOX = "document.querySelector('.agent-frame[data-id=\"%s\"] .xterm-helper-textarea')" % CELL
READY = ("(()=>{const f=document.querySelector('.agent-frame[data-id=\"%s\"] .frame-body'); const t=f&&f.xterm; if(!t) return false;"
         " const l=t.buffer.active.getLine(0); return !!l && l.translateToString(true).includes('READY');})()" % CELL)
TYPED = "MACHINE AGAIN TWO"
COMMIT = "SEVEN EIGHT"


def main() -> bool:
    rig = Rig(__file__, live=False)
    rig.build()
    rig.make_home(None)
    rig.proj.mkdir(parents=True, exist_ok=True)
    (rig.proj / "rec.py").write_text(REC)
    cell = {"id": CELL, "handler_module": "terminal_view.ts", "backend_id": BACKEND, "owns_backend": False,
            "command": ["python3", "rec.py"], "can_call": {"$self": "*", BACKEND: VERBS}, "callable_by": {"$self": "*"},
            "x": 40, "y": 40, "width": 600, "height": 300}
    host = {"id": BACKEND, "handler_module": "exec_bridge.tools", "cwd": ".", "inherit_env": ["*"], "shell": "login",
            "ingress_rule": "allow_all_ingress", "allowed": [r"^python3 rec\.py$"], "cell_id": CELL, "can_call": {"$self": "*"},
            "callable_by": {"$self": "*", "mcp": ["*"], "user": ["*"], "canvas_root": ["*"], "canvas_ws": ["watch"]}}
    rig.init(cells=[cell], host_agents=[(FOLDER, host)])
    log = rig.proj / "keys.log"
    r: dict = {}
    info: dict = {}

    def got() -> str:
        return log.read_bytes().decode(errors="replace") if log.is_file() else ""

    async def drive() -> None:
        async with rig.page(1400, 900) as pg:
            r["ready"] = bool(await pg.until(READY, 20))
            await pg.js(f"{BOX}.focus()")
            for ch in TYPED:
                await pg.cdp("Input.dispatchKeyEvent", type="keyDown", text=ch, key=ch, unmodifiedText=ch)
                await pg.cdp("Input.dispatchKeyEvent", type="keyUp", key=ch)
                await asyncio.sleep(0.03)
            await asyncio.sleep(0.8)
            info["typed"], info["box_after_typing"] = got(), await pg.js(f"{BOX}.value")
            r["typed_once"] = info["typed"] == TYPED
            r["box_empty_after_typing"] = info["box_after_typing"] == ""
            info["attrs"] = await pg.js(f"[{BOX}.getAttribute('writingsuggestions'),{BOX}.getAttribute('autocomplete')]")
            r["text_services_off"] = info["attrs"] == ["false", "off"]
            n = len(got())
            for h in ("SEV", "SEVEN", COMMIT):
                await pg.cdp("Input.imeSetComposition", text=h, selectionStart=len(h), selectionEnd=len(h))
                await asyncio.sleep(0.12)
            await pg.cdp("Input.insertText", text=COMMIT)
            await asyncio.sleep(0.8)
            info["committed"], info["box_after_commit"] = got()[n:], await pg.js(f"{BOX}.value")
            r["commit_once"] = info["committed"] == COMMIT
            r["box_empty_after_commit"] = info["box_after_commit"] == ""
            info["page_errors"] = [e for e in pg.logs if "EXC" in e][:3]

    try:
        asyncio.run(drive())
    finally:
        rig.stop()
    ok = bool(r) and all(r.values())
    rig.summary["checks"], rig.summary["info"] = r, info
    note = "typed and committed text reach the program once; the box stays empty" if ok else \
        f"failed: {', '.join(k for k, v in r.items() if not v)} · {info}"
    return verdict(*rig.finish(ok, note))


if __name__ == "__main__":
    sys.exit(0 if main() else 1)
