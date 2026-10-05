"""E57 setup_ai — the dedicated set-up test (owner: AI set-up is VERY important)."""

from __future__ import annotations

import asyncio
import os
import re
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[2]))
from _lib.canvas_live import seed_key, Rig, verdict, answer, ask, html_cell, http_get, in_frame, say, spot, wait_quiet  # noqa: E402,F401

def main() -> bool:
    rig = Rig(__file__)
    why = rig.plug_or_refusal()
    if why:
        return verdict(*rig.finish(False, why))
    rig.build()
    rig.make_home(None)  # NO template: the set-up makes it
    # the key as an earlier set-up leaves it (plain mode, the ref remembered,
    # its value in the 0600 file) — BEFORE the first boot, which makes the
    # credential store from it; the write then names it on the record. The
    # window comes with it (required on a vLLM record, never guessed: CTX)
    rec = seed_key(rig.home, rig.plug.record())
    rig.init()
    module = rec["handler_module"]
    import json

    async def drive():
        async with rig.page(1600, 1000) as pg:
            await spot(pg, 250, 250, "agent")
            await pg.until("document.querySelectorAll('.setup-view select option').length > 1", 20)
            await pg.js(f"(()=>{{const s=document.querySelector('.setup-view select'); s.value={json.dumps(module)}; s.dispatchEvent(new Event('change'));}})()")
            await asyncio.sleep(0.5)
            fill = {k: (v if isinstance(v, str) else json.dumps(v)) for k, v in rec.items() if k != "handler_module"}
            fill["id"] = "helper"
            filled = await pg.js("((f)=>{const got=[];for(const l of document.querySelectorAll('.setup-view label')){const n=l.querySelector('span')?.textContent;"
                                 "const i=l.querySelector('input,select'); if(n in f && i){i.value=f[n]; i.dispatchEvent(new Event('change')); got.push(n)}} return got})(" + json.dumps(fill) + ")")
            click = lambda label: pg.js(f"[...document.querySelectorAll('.setup-view button')].find(b=>b.textContent==={json.dumps(label)})?.click()")
            note = lambda: pg.js("document.querySelector('.setup-view .row .ok, .setup-view .row .err')?.textContent")
            await click("WRITE THE RECORD")
            await asyncio.sleep(2)
            written = await note()
            await pg.js("(()=>{const l=[...document.querySelectorAll('.setup-view label')].find(l=>l.querySelector('span')?.textContent==='name'); const i=l.querySelector('input'); i.value='mine';})()")
            await click("SAVE AS TEMPLATE")
            await asyncio.sleep(1.5)
            saved = await note()
            await pg.js("[...document.querySelectorAll('.setup-view ul.tpl li')].find(li=>li.querySelector('.name')?.textContent==='mine')?.querySelectorAll('button')[1].click()")
            await asyncio.sleep(1.5)
            default = await note()
            # the default template answers in a NEW cell
            done1, a1 = await __import__('_lib.canvas_live', fromlist=['ask']).ask(pg, 1150, 300, "Reply with exactly one word: hello",
                                                                                  rig.limits["total_seconds"], rig.limits["quiet_seconds"])
            # the written agent answers from the sidebar
            for _ in range(6):
                n = await pg.js("(()=>{const t=[...document.querySelectorAll('#chats .agents-tree li .tw')].filter(x=>x.textContent==='▸');t.forEach(x=>x.click());return t.length})()")
                await asyncio.sleep(0.8)
                if not n:
                    break
            before = await pg.js("document.querySelectorAll('.chat-view').length")
            await pg.js("[...document.querySelectorAll('#chats .agents-tree li.agent')].find(l=>l.querySelector('.nm')?.textContent==='helper')?.click()")
            await pg.until(f"document.querySelectorAll('.chat-view').length > {before}", 15)
            await asyncio.sleep(2)
            await say(pg, "Reply with exactly one word: ready", cell=before)
            await wait_quiet(pg, rig.limits["total_seconds"], rig.limits["quiet_seconds"], cell=before)
            a2 = await answer(pg, cell=before)
            return filled, written, saved, default, a1, a2
    try:
        filled, written, saved, default, a1, a2 = asyncio.run(drive())
    finally:
        rig.stop()
    tpl = (rig.home / "templates" / "mine.yaml").exists()
    pref = (rig.home / "templates" / ".preferred").read_text().strip() if (rig.home / "templates" / ".preferred").exists() else ""
    agent = rig.host_record("helper")
    rig.summary.update({"filled": filled, "written": written, "saved": saved, "default": default,
                        "template_file": tpl, "preferred": pref, "agent": bool(agent), "default_answer": a1, "helper_answer": a2})
    print(f"filled {filled}\nwrite: {written}\nsave: {saved}\ndefault: {default}\ntemplate file {tpl} · preferred {pref!r} · helper record {bool(agent)}")
    print(f"default-template cell answered: {a1[:120]!r}\nhelper answered: {a2[:120]!r}")
    ok = bool(agent) and tpl and pref == "mine" and bool(a1.strip()) and bool(a2.strip())
    return verdict(*rig.finish(ok, f"record written: {bool(agent)} · template saved + default: {tpl and pref == 'mine'} · both answer: {bool(a1.strip()) and bool(a2.strip())}"))


if __name__ == "__main__":
    sys.exit(0 if main() else 1)
