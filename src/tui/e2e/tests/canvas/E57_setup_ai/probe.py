"""E57 setup_ai — the dedicated set-up test (owner: AI set-up is VERY important),
live: the wizard makes the first AI agent from the BACKEND plug and it answers.

"AI agent" from the double-click → page 1: the plug's provider → page 2: its
URL, model and required fields from the plug's record, the key already stored
(an earlier set-up's, picked as the saved key for that host) → create & open
chat: the saved set-up is saved and made the default, and the new cell answers. Its
⚙ settings then show the same values."""

from __future__ import annotations

import asyncio
import json
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[2]))
from _lib.canvas_live import seed_key, Rig, verdict, answer, say, spot, wait_quiet, saved_dir  # noqa: E402

BTN = "((t)=>[...document.querySelectorAll('.wiz button')].find(b=>b.textContent.trim().startsWith(t)))"


def main() -> bool:
    rig = Rig(__file__)
    why = rig.plug_or_refusal()
    if why:
        return verdict(*rig.finish(False, why))
    rig.build()
    rig.make_home(None)  # NO saved set-up: the wizard makes it
    # the key as an earlier set-up leaves it (plain mode, the ref remembered,
    # its value in the 0600 file) — BEFORE the first boot, which makes the
    # credential store from it; the wizard then picks it as the saved key
    rec = seed_key(rig.home, rig.plug.record())
    rig.init()
    provider = rec["handler_module"].removesuffix("_backend.tools")
    # page 2's values: every record field by its label (`_` shown as a space)
    fill = {k.replace("_", " "): (v if isinstance(v, str) else json.dumps(v)) for k, v in rec.items() if k not in ("handler_module", "credentials")}
    fill["URL"] = fill.pop("endpoint", "")

    async def drive() -> dict:
        out: dict = {}
        async with rig.page(1600, 1000) as pg:
            await spot(pg, 250, 250, "AI agent")  # none set up: the wizard
            await pg.until("document.querySelectorAll('.wiz .tiles button').length === 4", 20)
            await pg.js(f"[...document.querySelectorAll('.wiz .tiles button')].find(b=>b.firstChild.textContent==={json.dumps(provider)})?.click()")
            await pg.until("!!document.querySelector('.wiz label input')", 10)
            out["filled"] = await pg.js("((f)=>{const got=[]; for (const l of document.querySelectorAll('.wiz label')) { const n=l.querySelector('span')?.textContent; const i=l.querySelector('input,select');"
                                        " if (n in f && i) { i.value=f[n]; i.dispatchEvent(new Event('input')); i.dispatchEvent(new Event('change')); got.push(n); } } return got; })(" + json.dumps(fill) + ")")
            out["key_source"] = await pg.js("(()=>{const l=[...document.querySelectorAll('.wiz label')].find(l=>l.querySelector('span').textContent==='key'); return l ? l.querySelector('select').value : null})()")
            before = await pg.js("document.querySelectorAll('.chat-view').length")
            await pg.js(f"{BTN}('create')?.click()")
            out["created"] = bool(await pg.until(f"!document.querySelector('.wiz') && document.querySelectorAll('.chat-view').length > {before}", 30))
            out["say"] = await pg.js("document.querySelector('.wiz .say')?.textContent")
            await pg.until(f"!!document.querySelectorAll('.chat-view')[{before}]?.querySelector('.c-model')?.textContent", 20)
            await asyncio.sleep(1.5)
            await say(pg, "Reply with exactly one word: hello", cell=before)
            await wait_quiet(pg, rig.budget(rig.limits["total_seconds"]), rig.limits["quiet_seconds"], cell=before)
            out["answer"] = await answer(pg, cell=before)
            # its ⚙ settings: the same values
            await pg.js(f"(()=>{{const f=document.querySelectorAll('.chat-view')[{before}].closest('.agent-frame'); [...f.querySelectorAll(':scope > .frame-head .ext')].find(c=>c.textContent==='⚙')?.click()}})()")
            await pg.until("(document.querySelector('.wiz h1')?.textContent||'').startsWith('Settings')", 15)
            out["settings_url"] = await pg.js("(()=>{const l=[...document.querySelectorAll('.wiz label')].find(l=>l.querySelector('span').textContent==='URL'); return l && l.querySelector('input').value})()")
        return out

    try:
        out = asyncio.run(drive())
    finally:
        rig.stop()
    name = f"{provider}-1"
    tpl = (saved_dir(rig.home) / f"{name}.yaml").exists()
    pref = (saved_dir(rig.home) / ".preferred").read_text().strip() if (saved_dir(rig.home) / ".preferred").exists() else ""
    rig.summary.update({**out, "saved_file": tpl, "preferred": pref})
    print(json.dumps(rig.summary, indent=1, default=str)[:2000])
    ok = out.get("created") and tpl and pref == name and bool((out.get("answer") or "").strip()) and out.get("settings_url") == fill["URL"]
    return verdict(*rig.finish(bool(ok), f"created: {out.get('created')} · saved set-up saved + default: {tpl and pref == name} · answers: {bool((out.get('answer') or '').strip())} · settings show its URL: {out.get('settings_url') == fill['URL']}"))


if __name__ == "__main__":
    sys.exit(0 if main() else 1)
