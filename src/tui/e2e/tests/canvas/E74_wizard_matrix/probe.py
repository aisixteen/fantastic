"""E74 wizard matrix — the AI set-up wizard × the four providers, on the provider
MOCKS (`e2e/mocks`, no model). For each provider, in one canvas:

  A. NEW provider, a PASTED key (kept in a plain file): the agent answers; every
     request it made carried that key.
  B. NEW provider at the same host: the key source starts on the SAVED key; the
     agent answers with it.
  C. NEW provider on a second mock that wants no key: "no key" — refused by the
     wizard when the backend requires a key, else the agent answers keyless.
  D. A FAILED TURN (the mock answers 500 on every request): the chat offers settings.
  E. ⚙ SETTINGS on A: a changed model is on the live record and on the next turn.
  F. A SAVED provider from page 1 (an AI cell's ⚙ → `‹ other set-ups`): the
     SAME cell is bound at once, answering.

The wizard for a new agent is the double-click's ONE `AI agent` tile: the
wizard itself while nothing is set up, else an AI cell on the default whose
⚙ → `‹ other set-ups` is page 1 for it (a create rebinds that cell)."""

from __future__ import annotations

import asyncio
import json
import sys
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[2]))
sys.path.insert(0, str(Path(__file__).resolve().parents[6] / "e2e"))
from _lib.canvas_live import Rig, new_ai_wizard, rebound, say, verdict  # noqa: E402
from mocks.anthropic import AnthropicMock  # noqa: E402
from mocks.ollama import OllamaMock  # noqa: E402
from mocks.responses import ResponsesMock  # noqa: E402
from mocks.vllm import VllmMock  # noqa: E402

import os
import subprocess
PROVIDERS = {"vllm": VllmMock, "ollama": OllamaMock, "responses": ResponsesMock, "anthropic": AnthropicMock}
if os.environ.get("E74_ONLY"):  # a comma list: run just these providers
    PROVIDERS = {k: v for k, v in PROVIDERS.items() if k in os.environ["E74_ONLY"].split(",")}
KEY = "sk-matrix-1"
FAILS: list = []
BTN = "((t)=>[...document.querySelectorAll('.wiz button')].find(b=>b.textContent.trim().startsWith(t)))"
CHATS = "[...document.querySelectorAll('.chat-view')].map(v=>v.closest('.agent-frame').dataset.id)"


async def wizard(pg, x: int, y: int) -> str | None:
    """Open the wizard for a new agent from the double-click at (x, y); the cell
    it will bind, or None (a create then makes a new cell)."""
    return await new_ai_wizard(pg, x, y)


async def page2(pg, provider: str, url: str, model: str, key: str) -> dict:
    """Page 1 → the provider's NEW tile → page 2 filled: URL, model, every required
    field, and the key source (`saved` | `new` | `none`). Returns what page 2 showed."""
    await pg.js(f"[...document.querySelectorAll('.wiz .tiles button')].find(b=>b.firstChild.textContent==={json.dumps(provider)}).click()")
    await pg.until("!!document.querySelector('.wiz label input')", 30)
    # no page 2 (it never came): what it showed is nothing, and every check on it fails
    shown = await pg.js("""(()=>{const l=[...document.querySelectorAll('.wiz label')].find(l=>l.querySelector('span').textContent==='key'); return {
        name: document.querySelector('.wiz label input').value,
        key_source: l ? l.querySelector('select').value : null,
        key_options: l ? [...l.querySelector('select').options].map(o=>o.value) : []}})()""") or {}
    await pg.js("""((url, model, key, secret)=>{
        const lab = (n)=>[...document.querySelectorAll('.wiz label')].find(l=>l.querySelector('span').textContent===n);
        const set = (n, v)=>{const l=lab(n); const i=l&&l.querySelector('input'); if(i){i.value=v; i.dispatchEvent(new Event('input'));}};
        set('URL', url); set('model', model);
        window.__keyAfterUrl = (lab('key')?.querySelector('select') || {}).value || null;
        for (const i of document.querySelectorAll('.wiz > label input')) if (!i.value && i.placeholder.includes('required')) i.value='262144';
        for (const s of document.querySelectorAll('.wiz > label select')) if (!s.value && s.options.length > 1 && s.options[0].textContent.includes('choose')) s.value = s.options[1].value;
        const k = lab('key'); const ks = k && k.querySelector('select');
        if (ks) { ks.value = key; ks.dispatchEvent(new Event('change')); }
        if (key === 'new') { set('value', secret); const keep = lab('keep it in'); if (keep) keep.querySelector('select').value = 'plain'; }
    })(""" + ",".join(json.dumps(v) for v in (url, model, key, KEY)) + ")")
    shown["key_after_url"] = await pg.js("window.__keyAfterUrl")
    return shown


async def create(pg, target: str | None) -> tuple[str | None, str]:
    """Press create; the chat cell's id — `target` rebound, or a new cell — (None
    if the wizard stayed) and what it said."""
    before = set(await pg.js(CHATS))
    await pg.js(f"{BTN}('create')?.click()")
    t0 = time.time()
    while time.time() - t0 < 8:
        if target is not None:
            if await rebound(pg, target, 0.3):
                return target, ""
        else:
            now = set(await pg.js(CHATS))
            if (now - before) and not await pg.js("!!document.querySelector('.wiz')"):
                return (now - before).pop(), ""
        said = await pg.js("document.querySelector('.wiz .say')?.textContent || ''")
        if said.startswith("✕"):
            return None, said
        await asyncio.sleep(0.3)
    return None, await pg.js("document.querySelector('.wiz .say')?.textContent || ''")


async def turn(pg, cell: str, mock, text: str, expect: str) -> bool:
    """Queue `expect` on the mock, send `text` in `cell`; True once the answer is in its dialog."""
    await pg.until(f"!!document.querySelector('.agent-frame[data-id=\"{cell}\"] .chat-view[data-ready]')", 10)
    mock.reply(expect)
    n = await pg.js(f"{CHATS}.indexOf({json.dumps(cell)})")
    await say(pg, text, cell=n)
    ok = bool(await pg.until(f"(document.querySelector('.agent-frame[data-id=\"{cell}\"] .c-dialog')?.innerText||'').includes({json.dumps(expect)})", 12))
    if not ok:
        FAILS.append({"cell": cell, "expect": expect, "dialog": (await pg.js(f"document.querySelector('.agent-frame[data-id=\"{cell}\"] .c-dialog')?.innerText") or "")[-500:],
                      "mock": [(q.get("method"), q.get("path"), q.get("answered"), q.get("key")) for q in mock.requests[-6:]]})
    return ok


def settings_evidence(rig: Rig, ai: str | None, sent: float, seen: float | None, window_end: float) -> dict:
    """#96 evidence: when the settings change was sent, when the record file
    showed it, what the host's agent record answers, and what persist logged."""
    ev: dict = {"sent": round(sent, 3), "disk_seen_after_s": None if seen is None else round(seen - sent, 3),
                "window_end_after_s": round(window_end - sent, 3)}
    if ai:
        t = time.time()
        try:
            p = subprocess.run([str(rig.binary), "send", json.dumps({"target": ai, "payload": {"type": "reflect"}})],
                               cwd=rig.proj, env=rig.env(), capture_output=True, text=True, timeout=30)
            try:
                ev["host_model"] = (json.loads(p.stdout).get("result") or {}).get("model")
            except ValueError:
                ev["host_error"] = (p.stdout + p.stderr)[-300:]
        except subprocess.TimeoutExpired:
            ev["host_error"] = "reflect timed out after 30 s"
        ev["host_asked_after_s"] = round(t - sent, 3)
        ev["host_answer_took_s"] = round(time.time() - t, 3)
    try:
        ev["persist_errors"] = [ln for ln in rig.log.read_text(errors="replace").splitlines() if "persist through" in ln]
    except OSError as e:
        ev["persist_errors"] = f"host log unreadable: {e}"
    return ev


def main() -> bool:
    T0 = time.time()
    rig = Rig(__file__, live=False)
    rig.build("vllm", "ollama", "responses", "anthropic")
    rig.make_home(None)
    timing: dict = {"build": round(time.time() - T0, 1)}
    r: dict = {}
    info: dict = {}
    mocks: list = []
    try:
        t = time.time()
        rig.init()
        timing["init"] = round(time.time() - t, 1)

        async def run() -> None:
            t = time.time()
            async with rig.page(1800, 1100) as pg:
                timing["page"] = round(time.time() - t, 1)
                mark = [time.time()]

                def lap(name: str) -> None:
                    now = time.time()
                    timing[name] = round(now - mark[0], 1)
                    print(f"E74 step {name}: {timing[name]}s", flush=True)
                    mark[0] = now
                for i, (provider, Mock) in enumerate(PROVIDERS.items()):
                    p: dict = {}
                    info[provider] = p
                    keyed = Mock(model="mock-x").start()
                    keyed.require_key(KEY)
                    open_ = Mock(model="mock-x").start()
                    mocks.extend([keyed, open_])
                    base_y = 100 + i * 40
                    # A. a pasted key
                    at = await wizard(pg, 200, base_y)
                    p["A_page2"] = await page2(pg, provider, keyed.url, "mock-x", "new")
                    a, p["A_say"] = await create(pg, at)
                    p["A_answers"] = bool(a) and await turn(pg, a, keyed, "hi", f"answer-A-{provider}")
                    lap(f"{provider}.A")
                    p["A_keys"] = sorted({k for k in keyed.keys() if k is not None} | ({None} if None in keyed.keys() else set()), key=str)
                    # B. the same host: the saved key
                    at = await wizard(pg, 700, base_y)
                    p["B_page2"] = await page2(pg, provider, keyed.url, "mock-x", "saved")
                    b, p["B_say"] = await create(pg, at)
                    p["B_answers"] = bool(b) and await turn(pg, b, keyed, "hi", f"answer-B-{provider}")
                    lap(f"{provider}.B")
                    # C. no key, on the open mock
                    at = await wizard(pg, 1200, base_y)
                    p["C_page2"] = await page2(pg, provider, open_.url, "mock-x", "none")
                    c, p["C_say"] = await create(pg, at)
                    if c is None:
                        p["C_refused"] = "a key" in p["C_say"]
                        await pg.js(f"{BTN}('skip')?.click()")  # leave the wizard
                        await asyncio.sleep(1)
                        for f in await pg.js("[...document.querySelectorAll('.ai-not-set-up')].map(x=>x.closest('.agent-frame').dataset.id)"):
                            # Shift+✕ deletes the cell (✕ alone only hides it)
                            await pg.js(f"document.querySelector('.agent-frame[data-id=\"{f}\"] > .frame-head .close')?.dispatchEvent(new MouseEvent('click',{{bubbles:true,shiftKey:true}}))")
                    else:
                        p["C_answers"] = await turn(pg, c, open_, "hi", f"answer-C-{provider}")
                        p["C_keys"] = [k for k in open_.keys()]
                    lap(f"{provider}.C")
                    # D. a failed turn offers settings
                    if a:
                        keyed.error(500, times=None)
                        await pg.until(f"!!document.querySelector('.agent-frame[data-id=\"{a}\"] .chat-view[data-ready]')", 10)
                        n = await pg.js(f"{CHATS}.indexOf({json.dumps(a)})")
                        await say(pg, "fail please", cell=n)
                        p["D_offers_settings"] = bool(await pg.until(f"!!document.querySelector('.agent-frame[data-id=\"{a}\"] .c-setup-help')", 20))
                        keyed.error(None)
                        # E. ⚙ settings: a changed model, live
                        await pg.js(f"[...document.querySelectorAll('.agent-frame[data-id=\"{a}\"] > .frame-head .ext')].find(c=>c.textContent==='⚙')?.click()")
                        # (a slow runner opens and saves the settings in seconds: the bounds are wide)
                        await pg.until("(document.querySelector('.wiz h1')?.textContent||'').startsWith('Settings')", 20)
                        await pg.js("(()=>{const l=[...document.querySelectorAll('.wiz label')].find(l=>l.querySelector('span').textContent==='model'); l.querySelector('input').value='mock-y';})()")
                        e_sent = time.time()
                        await pg.js(f"{BTN}('save')?.click()")
                        p["E_saved"] = bool(await pg.until("(document.querySelector('.wiz .say')?.textContent||'').includes('saved')", 20))
                        await pg.until("!document.querySelector('.wiz')", 10)
                        cellrec = next((x for x in rig.cells() if x.get("id") == a), {})
                        ai = cellrec.get("backend_id")
                        # the 1.5 s window, polled: when the record file first shows the change (evidence only)
                        e_seen = None
                        e_end = time.time() + 1.5
                        while True:
                            e_model = (rig.host_record(ai) or {}).get("model") if ai else None
                            if e_seen is None and e_model == "mock-y":
                                e_seen = time.time()
                            left = e_end - time.time()
                            if left <= 0:
                                break
                            await asyncio.sleep(min(0.1, left))
                        p["E_record_model"] = e_model  # the window's last poll: the assertion reads where the sleep ended
                        # after the assertion's read, so the host's answer and log add no wait to it
                        p["E_evidence"] = settings_evidence(rig, ai, e_sent, e_seen, e_end)
                    lap(f"{provider}.DE")
                    # F. a saved provider from page 1: the SAME cell rebound at once
                    f = await wizard(pg, 200, base_y + 600)
                    saved = await pg.js("[...document.querySelectorAll('.wiz .saved button .name')].map(n=>n.textContent)")
                    p["F_saved_list"] = saved
                    await pg.js(f"[...document.querySelectorAll('.wiz .saved button')].find(b=>b.querySelector('.name')?.textContent==={json.dumps(provider + '-1')})?.click()")
                    p["F_bound_same_cell"] = f is not None and await rebound(pg, f)
                    p["F_answers"] = p["F_bound_same_cell"] and await turn(pg, f, keyed, "hi", f"answer-F-{provider}")
                    lap(f"{provider}.F")
                    p["keyed_keys_seen"] = sorted({str(k) for k in keyed.keys()})
                    p["E_next_turn_model"] = next((q["body"].get("model") for q in reversed(keyed.requests) if q.get("kind") == "turn" and isinstance(q.get("body"), dict)), None)
                    # tidy the canvas for the next provider: Shift+✕ deletes each cell (✕ would only hide it, agent running)
                    for fid in await pg.js("[...document.querySelectorAll('.agent-frame')].map(f=>f.dataset.id)"):
                        await pg.js(f"document.querySelector('.agent-frame[data-id=\"{fid}\"] > .frame-head .close')?.dispatchEvent(new MouseEvent('click',{{bubbles:true,shiftKey:true}}))")
                    await asyncio.sleep(0.5)
                info["console"] = [x for x in pg.logs if "WebGL" not in x][:8]

        asyncio.run(run())
        for provider in PROVIDERS:
            p = info.get(provider, {})
            r[f"{provider}: A pasted key answers"] = p.get("A_answers") is True
            r[f"{provider}: A key on every request"] = p.get("keyed_keys_seen") == [KEY]
            r[f"{provider}: B starts on the saved key and answers"] = (p.get("B_page2") or {}).get("key_after_url") == "saved" and p.get("B_answers") is True
            r[f"{provider}: C no key: answers keyless, or refused when required"] = p.get("C_refused") is True or (p.get("C_answers") is True and set(p.get("C_keys") or []) == {None})
            r[f"{provider}: D failed turn offers settings"] = p.get("D_offers_settings") is True
            r[f"{provider}: E settings change is live"] = p.get("E_saved") is True and p.get("E_record_model") == "mock-y"
            r[f"{provider}: F saved provider binds the same cell and answers"] = f"{provider}-1" in (p.get("F_saved_list") or []) and p.get("F_answers") is True
    finally:
        for m in mocks:
            m.stop()
        rig.stop()
    rig.summary["checks"] = r
    rig.summary["info"] = info
    timing["total"] = round(time.time() - T0, 1)
    info["timing"] = timing
    info["fails"] = FAILS
    print(json.dumps(info, indent=1, default=str)[:6000])
    failed = [k for k, v in r.items() if not v]
    for provider in PROVIDERS:
        if not r.get(f"{provider}: E settings change is live", True):
            print(f"E74 #96 evidence {provider}: {json.dumps(info.get(provider, {}).get('E_evidence'), default=str)}", flush=True)
    return verdict(*rig.finish(not failed, f"{len(r) - len(failed)}/{len(r)} pass" + ("" if not failed else f" — failed: {'; '.join(failed)}")))


if __name__ == "__main__":
    sys.exit(0 if main() else 1)
