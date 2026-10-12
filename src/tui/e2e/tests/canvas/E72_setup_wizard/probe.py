"""E72 setup wizard — the AI set-up as a two-page wizard (owner 2026-10-11),
free: the endpoint is unreachable on purpose (no model is called).

1. "AI agent" from the double-click opens page 1 (saved providers + the four new
   ones); SKIP leaves an AI cell that is "not set up yet".
2. That cell's ⚙ settings runs the wizard FOR it: vllm → page 2 (name auto,
   URL filled) → create: the template is saved and made the default, and the
   SAME cell is bound to the new AI (the wizard closes).
3. A turn fails (nothing listens at the URL): the chat offers settings.
4. The cell's ⚙ chip opens its settings (page 2, its values); a changed model
   is saved on the live AI's record."""

from __future__ import annotations

import asyncio
import json
import sys
from pathlib import Path

import yaml

sys.path.insert(0, str(Path(__file__).resolve().parents[2]))
from _lib.canvas_live import Rig, say, spot, verdict  # noqa: E402

BTN = "((t)=>[...document.querySelectorAll('.wiz button')].find(b=>b.textContent.trim().startsWith(t)))"


def main() -> bool:
    rig = Rig(__file__, live=False)
    rig.build("vllm")
    rig.make_home(None)  # no template: the wizard makes the first
    r: dict = {}
    info: dict = {}
    try:
        rig.init()

        async def run() -> None:
            async with rig.page(1500, 950) as pg:
                click = lambda t: pg.js(f"{BTN}({json.dumps(t)})?.click()")
                # 1. page 1, then skip
                await spot(pg, 300, 300, "new AI agent…")
                await pg.until("document.querySelectorAll('.wiz .tiles button').length === 4", 20)
                info["tiles"] = await pg.js("[...document.querySelectorAll('.wiz .tiles button')].map(b=>[b.firstChild.textContent, b.disabled])")
                info["lead"] = await pg.js("document.querySelector('.wiz .lead')?.textContent")
                info["tip"] = await pg.js("document.querySelector('.wiz .tip')?.textContent")
                # opened at its own size where it was asked for (a fresh project's first
                # cell once lost its rect to a refresh and came up 480×320)
                info["wizard_size"] = await pg.js("(()=>{const f=document.querySelector('.wiz').closest('.agent-frame'); return [f.style.width, f.style.height]})()")
                await click("skip")
                await pg.until("!!document.querySelector('.ai-not-set-up') && !document.querySelector('.wiz')", 15)
                info["not_set_up"] = await pg.js("document.querySelector('.ai-not-set-up p')?.textContent")
                cell = await pg.js("document.querySelector('.ai-not-set-up').closest('.agent-frame').dataset.id")
                info["cell"] = cell
                # 2. its settings: the wizard for it
                await pg.js("document.querySelector('.ai-not-set-up .nsu-settings').click()")
                await pg.until("document.querySelectorAll('.wiz .tiles button').length === 4", 20)
                await pg.js("[...document.querySelectorAll('.wiz .tiles button')].find(b=>b.firstChild.textContent==='vllm').click()")
                await pg.until("!!document.querySelector('.wiz label input')", 10)
                vals = "[...document.querySelectorAll('.wiz label')].map(l=>[l.querySelector('span').textContent, (l.querySelector('input,select')||{}).value])"
                info["page2"] = await pg.js(vals)
                # the URL nothing listens at, a model, every required field
                await pg.js("""(()=>{const set=(n,v)=>{const l=[...document.querySelectorAll('.wiz label')].find(l=>l.querySelector('span').textContent===n); const i=l&&l.querySelector('input'); if(i){i.value=v; i.dispatchEvent(new Event('input'));}};
                  set('URL','http://127.0.0.1:9/v1'); set('model','x');
                  for (const i of document.querySelectorAll('.wiz label input')) if (!i.value && i.placeholder.includes('required')) i.value='32000';
                  for (const s of document.querySelectorAll('.wiz > label select')) if (!s.value && s.options.length > 1 && s.options[0].textContent.includes('choose')) s.value = s.options[1].value;
                  const k=[...document.querySelectorAll('.wiz label')].find(l=>l.querySelector('span').textContent==='key'); const s=k&&k.querySelector('select'); if(s){s.value='none'; s.dispatchEvent(new Event('change'));}})()""")
                # a URL with no scheme is refused, naming what it needs
                await pg.js("(()=>{const l=[...document.querySelectorAll('.wiz label')].find(l=>l.querySelector('span').textContent==='URL'); const i=l.querySelector('input'); i.value='127.0.0.1:9/v1'; i.dispatchEvent(new Event('input'));})()")
                await click("create")
                await pg.until("(document.querySelector('.wiz .say')?.textContent||'').includes('http://')", 5)
                info["no_scheme_say"] = await pg.js("document.querySelector('.wiz .say')?.textContent")
                await pg.js("(()=>{const l=[...document.querySelectorAll('.wiz label')].find(l=>l.querySelector('span').textContent==='URL'); const i=l.querySelector('input'); i.value='http://127.0.0.1:9/v1'; i.dispatchEvent(new Event('input'));})()")
                await click("create")
                bound = await pg.until(f"!document.querySelector('.wiz') && !!document.querySelector('.agent-frame[data-id=\"{cell}\"] .chat-view')", 20)
                info["say_after_create"] = await pg.js("document.querySelector('.wiz .say')?.textContent")
                r["create_binds_the_same_cell"] = bool(bound)
                # 3. a failed turn offers settings (once the chat is mounted: its input listens)
                await pg.until(f"!!document.querySelector('.agent-frame[data-id=\"{cell}\"] .c-model')?.textContent", 15)
                await asyncio.sleep(1.5)
                n = await pg.js("[...document.querySelectorAll('.chat-view')].indexOf(document.querySelector('.agent-frame[data-id=\"" + cell + "\"] .chat-view'))")
                await say(pg, "hello", cell=max(n, 0))
                r["failed_turn_offers_settings"] = bool(await pg.until(f"!!document.querySelector('.agent-frame[data-id=\"{cell}\"] .c-setup-help')", 60))
                info["dialog"] = await pg.js(f"document.querySelector('.agent-frame[data-id=\"{cell}\"] .c-dialog')?.innerText?.slice(-400)")
                # 4. ⚙ on its head: the settings, its values; a changed model saved live
                await pg.js(f"[...document.querySelectorAll('.agent-frame[data-id=\"{cell}\"] > .frame-head .ext')].find(c=>c.textContent==='⚙')?.click()")
                await pg.until("!!document.querySelector('.wiz h1') && document.querySelector('.wiz h1').textContent.startsWith('Settings')", 15)
                info["settings_title"] = await pg.js("document.querySelector('.wiz h1').textContent")
                info["settings_values"] = await pg.js(vals)
                await pg.js("(()=>{const l=[...document.querySelectorAll('.wiz label')].find(l=>l.querySelector('span').textContent==='model'); l.querySelector('input').value='y';})()")
                await click("save")
                await pg.until("(document.querySelector('.wiz .say')?.textContent||'').includes('saved')", 15)
                info["saved_say"] = await pg.js("document.querySelector('.wiz .say')?.textContent")
                await asyncio.sleep(2.0)
                # EDIT the saved provider: its template rewritten under its name
                await pg.until("!document.querySelector('.wiz')", 10)
                await spot(pg, 900, 600, "new AI agent…")
                await pg.until("document.querySelectorAll('.wiz .saved-row').length > 0", 10)
                await pg.js("[...document.querySelectorAll('.wiz .saved-row')].find(r=>r.querySelector('.name')?.textContent==='vllm-1')?.querySelector('button.edit')?.click()")
                await pg.until("(document.querySelector('.wiz h1')?.textContent||'').startsWith('Edit saved')", 8)
                await pg.js("(()=>{const l=[...document.querySelectorAll('.wiz label')].find(l=>l.querySelector('span').textContent==='model'); l.querySelector('input').value='edited-model';})()")
                await click("save")
                await pg.until("(document.querySelector('.wiz .say')?.textContent||'').includes('saved')", 8)
                info["edit_say"] = await pg.js("document.querySelector('.wiz .say')?.textContent")
                info["console"] = [x for x in pg.logs if "WebGL" not in x][:8]

        asyncio.run(run())
        tiles = dict(info.get("tiles") or [])
        r["page1_four_providers_vllm_linked"] = list(tiles) == ["responses", "anthropic", "ollama", "vllm"] and tiles.get("vllm") is False
        r["lead_and_tip"] = (info.get("lead") or "").startswith("You can set everything up manually") and "lookaround" in (info.get("tip") or "")
        r["wizard_opens_at_its_size"] = info.get("wizard_size") == ["900px", "640px"]
        r["skip_leaves_not_set_up_cell"] = "Press settings to set me up" in (info.get("not_set_up") or "")
        p2 = dict(info.get("page2") or [])
        r["page2_name_auto_url_filled"] = p2.get("name") == "vllm-1" and p2.get("URL") == "http://127.0.0.1:8000/v1"
        tpl = rig.home / "templates" / "vllm-1.yaml"
        pref = rig.home / "templates" / ".preferred"
        r["template_saved_and_default"] = tpl.exists() and pref.exists() and pref.read_text().strip() == "vllm-1"
        sv = dict(info.get("settings_values") or [])
        r["settings_show_its_values"] = (info.get("settings_title") or "").startswith("Settings · vllm-1") and sv.get("URL") == "http://127.0.0.1:9/v1" and sv.get("model") == "x"
        cellrec = next((c for c in rig.cells() if c.get("id") == info.get("cell")), {})
        ai = cellrec.get("backend_id")
        info["ai"] = ai
        rec = rig.host_record(ai) if ai else None
        info["ai_model"] = (rec or {}).get("model")
        r["url_without_scheme_refused"] = "http://" in (info.get("no_scheme_say") or "")
        tdoc = yaml.safe_load((rig.home / "templates" / "vllm-1.yaml").read_text()) if (rig.home / "templates" / "vllm-1.yaml").exists() else {}
        info["template_model"] = tdoc.get("model")
        r["saved_provider_edit_rewrites_template"] = "saved" in (info.get("edit_say") or "") and tdoc.get("model") == "edited-model"
        r["settings_save_live"] = "saved" in (info.get("saved_say") or "") and (rec or {}).get("model") == "y"
    finally:
        rig.stop()
    rig.summary["checks"] = r
    rig.summary["info"] = info
    failed = [k for k, v in r.items() if not v]
    print(json.dumps(info, indent=1, default=str)[:3000])
    return verdict(*rig.finish(not failed, "all checks pass" if not failed else f"failed: {', '.join(failed)}"))


if __name__ == "__main__":
    sys.exit(0 if main() else 1)
