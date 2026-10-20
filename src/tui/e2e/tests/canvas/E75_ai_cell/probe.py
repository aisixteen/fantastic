"""E75 AI cell — the AI cell's SUNSHINE paths, on the vLLM mock (no model):

  S1 KNOWS THE KERNEL OUT OF THE BOX: its very first request already carries the
     kernel's description (the call grammar, its canvas, how to address agents)
     — no `lookaround` turn first (a CLI coding agent needs one; this agent not).
  S2 AUTO-CONNECTED: the first reply is a CALL to its canvas (`reflect` on the
     view) — it reaches it with no set-up, the result comes back as the next turn,
     and the chat shows the call; then it answers.
  S3 ANSWERS a plain turn.
  S4 QUEUES: a second message sent while the first is in flight waits, then is answered.
  S5 STOPS: a turn stopped while the model is still thinking settles idle, unanswered;
     the next turn answers.
  S6 KEPT FROM THE START: no keep button (no ephemeral chats); the page reloaded →
     the dialog's history is back.
  S7 REOPENS: the kept dialog's cell closed → the popup's chats tile → a new cell
     with the same history.
  S9 RUNS A SUBPROCESS: through an exec agent — start a ticker, MONITOR it (a
     delivery wakes the agent at "tick 2"), check its status, read its output so
     far, stop it; the process is gone.
  S10 ASKS THE PERSON: an `inquiry` to `user` (a single choice) shows as a card in
     its chat; a click answers it, and the agent continues with that answer.
  S8 FOLDS ONCE: an agent at a small window, a few long turns → one fold — the
     chat says the context was folded, and the next turn still answers."""

from __future__ import annotations

import asyncio
import json
import os
import re
import shlex
import sys
import time
from pathlib import Path

import yaml

sys.path.insert(0, str(Path(__file__).resolve().parents[2]))
sys.path.insert(0, str(Path(__file__).resolve().parents[6] / "e2e"))
from _lib.canvas_live import FOLDER, PY, Rig, other_setups, private_write, rebound, say, spot, verdict, saved_dir  # noqa: E402
from helpers import oslayer  # noqa: E402
from mocks import DONE  # noqa: E402
from mocks.vllm import VllmMock  # noqa: E402

CHATS = "[...document.querySelectorAll('.chat-view')].map(v=>v.closest('.agent-frame').dataset.id)"
# a point on empty canvas (no cell, no floating UI under it), scanned on a grid
EMPTY_POINT = ("(()=>{for (let y=120; y<innerHeight-40; y+=40) for (let x=60; x<innerWidth-60; x+=40) {"
               "const e=document.elementFromPoint(x,y); if (e && !e.closest('.agent-frame, .action-popup, .side, button, input')) return [x,y];}"
               " return [300,700];})()")


def last_text(req: dict) -> str:
    """The newest message of a request, as text."""
    msgs = req.get("messages") or [{}]
    c = msgs[-1].get("content", "")
    return c if isinstance(c, str) else json.dumps(c)


def is_nudge(req: dict) -> bool:
    """A turn the RUNTIME starts after a plain answer (no call): its no-call
    reminder, alone, as the newest message."""
    return last_text(req).lstrip().startswith(("[no call was dispatched]", "[no answer]"))


class SlowVllm(VllmMock):
    """The vLLM mock, held `stall` seconds before a turn's first byte (the model
    "thinking") — what a stop or a queued message needs to happen during. Its
    usage reports the prompt's real size (≈4 characters a token), not a fixed one:
    the fold trigger calibrates on the reported prompt size.

    THE NUDGE ENDS THE CHAIN AT ONCE: after every plain answer the runtime sends
    its no-call reminder as a turn of its own. Answered from the queue, it took
    the NEXT step's reply whenever it reached the mock after that step queued it
    (a slow runner): S4's first answer, S5's stalled one (the stop then cut the
    reminder, not the turn it meant). It gets `done`, unstalled, and takes nothing."""

    stall = 0.0
    nudges = 0

    def next_text(self, req):
        if self.responder is not None:
            got = self.responder(req)
            if got is not None:
                return got
        if is_nudge(req):
            self.nudges += 1
            return DONE
        with self.lock:
            return self._replies.pop(0) if self._replies else DONE

    def turn(self, h, req, text):
        self.prompt_tokens = max(1, len(json.dumps(req.get("messages", []))) // 4)
        if self.stall and not is_nudge(req):
            time.sleep(self.stall)
        return super().turn(h, req, text)


def saved_record(url: str, **extra) -> str:
    return yaml.safe_dump({"handler_module": "vllm_backend.tools", "endpoint": url, "model": "mock-x", "kv_overflow": "evict", **extra}, sort_keys=False)


def dialog(cell: str) -> str:
    return f"(document.querySelector('.agent-frame[data-id=\"{cell}\"] .c-dialog')?.innerText||'')"


async def until_turn(mock: VllmMock, words: str, secs: float) -> bool:
    """The mock has received the turn whose newest message carries `words`."""
    end = time.time() + secs
    while time.time() < end:
        if any(words in last_text(t.get("body", {})) for t in mock.turns()):
            return True
        await asyncio.sleep(0.1)
    return False


async def ready(pg, cell: str) -> None:
    await pg.until(f"!!document.querySelector('.agent-frame[data-id=\"{cell}\"] .chat-view[data-ready]')", 10)


async def send(pg, cell: str, text: str) -> None:
    n = await pg.js(f"{CHATS}.indexOf({json.dumps(cell)})")
    await say(pg, text, cell=n)


def main() -> bool:
    mock = SlowVllm(model="mock-x").start()
    # THE KERNEL'S FOLD ARM (e2e/helpers/facts_arm FOLD_ARMS, E2E_WINDOW): fold strategy,
    # async + titled folds, a 12000 window — a fold fires at 0.95 of the body
    # (window − output reserve − system − 160), the summary is a side call
    W = int(os.environ.get("E2E_WINDOW", "12000"))
    small = SlowVllm(model="mock-s", context_window=W, max_tokens=W).start()
    rig = Rig(__file__, live=False)
    rig.build("vllm")
    rig.make_home(None)
    # two saved set-ups: the default on the mock, and a small-window one
    private_write(saved_dir(rig.home) / "mock.yaml", saved_record(mock.url))
    private_write(saved_dir(rig.home) / "small.yaml", saved_record(small.url, context_window=W, max_tokens=W, context_strategy="fold", context_async=True, context_titles=True))
    private_write(saved_dir(rig.home) / ".preferred", "mock")
    r: dict = {}
    info: dict = {}
    try:
        # an exec agent the AI may run the ticker on (exactly that command)
        rig.init(host_agents=[(FOLDER, {"id": "svc", "handler_module": "exec_bridge.tools", "parent_id": "canvas_root",
                                         "allowed": ["^" + re.escape(shlex.quote(PY)) + r" -c .*E75TICK.*$"], "cwd": ".", "inherit_env": ["*"],
                                         "ingress_rule": "allow_all_ingress"})])
        view = f"{rig.page_guid()}|view_root"

        async def run() -> None:
            async with rig.page(1600, 1000) as pg:
                # S2 first: the first reply is a call to its canvas, then words
                mock.reply(f'<call name="send">\n<target_id>{view}</target_id>\n<type>reflect</type>\n</call>', "S2-done: I see my canvas.")
                await spot(pg, 300, 300, "hello there")
                await pg.until(f"{CHATS}.length === 1", 15)
                a = (await pg.js(CHATS))[0]
                info["cell"] = a
                r["S2 answers after its canvas call"] = bool(await pg.until(f"{dialog(a)}.includes('S2-done')", 15))
                info["S2_dialog"] = (await pg.js(dialog(a)))[-400:]
                turns = mock.turns()
                info["turn_count_S2"] = len(turns)
                # S1: the first request's system block, as the model got it
                first = turns[0]["body"] if turns else {}
                sysmsg = " ".join(m.get("content", "") if isinstance(m.get("content"), str) else json.dumps(m.get("content")) for m in first.get("messages", []) if m.get("role") == "system")
                info["S1_system_chars"] = len(sysmsg)
                info["S1_markers"] = {k: (k in sysmsg) for k in ("<call", "target_id", "reflect", "kernel", view)}
                r["S1 knows the kernel out of the box"] = len(sysmsg) > 500 and all(info["S1_markers"].values())
                second = turns[1]["body"] if len(turns) > 1 else {}
                r["S2 its canvas call came back as the next turn"] = "view_root" in json.dumps(second.get("messages", [])[-1:])
                await ready(pg, a)
                # S3 a plain turn
                mock.reply("S3-answer")
                await send(pg, a, "plain question")
                r["S3 answers"] = bool(await pg.until(f"{dialog(a)}.includes('S3-answer')", 10))
                # S4 queue: the first held 2 s, the second sent meanwhile
                mock.stall = 2.0
                mock.reply("S4-first", "S4-second")
                await send(pg, a, "first of two")
                await asyncio.sleep(0.4)
                await send(pg, a, "second of two")
                info["S4_queued_seen"] = bool(await pg.until(f"!!document.querySelector('.agent-frame[data-id=\"{a}\"] .c-queued .msg')", 3))
                r["S4 queues then answers both, in order"] = bool(await pg.until(
                    f"{dialog(a)}.includes('S4-second') && {dialog(a)}.indexOf('S4-first') < {dialog(a)}.indexOf('S4-second')", 15)) and info["S4_queued_seen"]
                # S5 stop while it thinks
                mock.stall = 6.0
                mock.reply("S5-never")
                await send(pg, a, "stop me")
                # THIS turn is the one at the model (stalled there) before the stop
                info["S5_at_model"] = await until_turn(mock, "stop me", 10)
                # in flight (thinking): the send button is STOP now
                info["S5_thinking"] = bool(await pg.until(f"document.querySelector('.agent-frame[data-id=\"{a}\"] .phase-pill')?.dataset.phase === 'thinking'", 5))
                info["S5_button"] = await pg.js(f"document.querySelector('.agent-frame[data-id=\"{a}\"] .c-send')?.textContent")
                await pg.js(f"document.querySelector('.agent-frame[data-id=\"{a}\"] .c-send').click()")
                idle = await pg.until(f"document.querySelector('.agent-frame[data-id=\"{a}\"] .phase-pill')?.dataset.phase === 'idle'", 8)
                # what the pill says instead (a CI-only miss: the diagnosis)
                info["S5_pill"] = await pg.js(f"(p=>p&&[p.dataset.phase,p.dataset.reason,p.title])(document.querySelector('.agent-frame[data-id=\"{a}\"] .phase-pill'))")
                await asyncio.sleep(6.5)
                mock.stall = 0.0
                info["S5_dialog_has_never"] = await pg.js(f"{dialog(a)}.includes('S5-never')")
                mock.clear()
                mock.reply("S5-after")
                await send(pg, a, "after the stop")
                t5 = time.time()
                after = bool(await pg.until(f"{dialog(a)}.includes('S5-after')", 10))
                info["S5_idle"], info["S5_after_s"] = bool(idle), round(time.time() - t5, 2) if after else None
                r["S5 stops, then answers the next"] = bool(idle) and not info["S5_dialog_has_never"] and after
                # S6 keep, reload: the history is back
                # every dialog is kept from its start: no keep button, the badge is its name
                info["S6_keep_button"] = await pg.js(f"!!document.querySelector('.agent-frame[data-id=\"{a}\"] .c-keep')")
                kept = await pg.until(f"!!(document.querySelector('.agent-frame[data-id=\"{a}\"] .c-badge')?.textContent||'').trim()", 8) and not info["S6_keep_button"]
                name = (await pg.js(f"document.querySelector('.agent-frame[data-id=\"{a}\"] .c-badge')?.textContent") or "").strip()
                info["kept_name"] = name
                await asyncio.sleep(1.5)
                await pg.open(rig.url)
                await pg.until(f"!!document.querySelector('.agent-frame[data-id=\"{a}\"] .chat-view[data-ready]')", 15)
                r["S6 keep survives a reload with its history"] = bool(kept) and bool(await pg.until(f"{dialog(a)}.includes('S3-answer') && {dialog(a)}.includes('S5-after')", 10))
                # S7 close the cell (✕ hides it; the kept dialog's agent lives on), reopen the kept dialog from the popup's chats
                await pg.js(f"document.querySelector('.agent-frame[data-id=\"{a}\"] > .frame-head .close').click()")
                await pg.until(f"!document.querySelector('.agent-frame[data-id=\"{a}\"]')", 8)
                # the popup's CHATS tile for that dialog (Enter would pick the first tile: "ask")
                for c_ in (1, 2):
                    await pg.mouse("mousePressed", 900, 300, c_)
                    await pg.mouse("mouseReleased", 900, 300, c_)
                await pg.until("!!document.querySelector('.action-popup input')", 10)
                await pg.cdp("Input.insertText", text=name)
                await asyncio.sleep(0.3)
                info["S7_tiles"] = await pg.js("[...document.querySelectorAll('.action-popup .tile')].map(t=>[t.querySelector('.lb')?.textContent, t.querySelector('.ht')?.textContent])")
                # a tile is picked on MOUSEDOWN (the popup's own handler)
                await pg.js(f"[...document.querySelectorAll('.action-popup .tile')].find(t=>t.querySelector('.lb')?.textContent==={json.dumps(name)})?.dispatchEvent(new MouseEvent('mousedown',{{bubbles:true}}))")
                await pg.until(f"{CHATS}.length === 1", 15)
                b = ((await pg.js(CHATS)) or [""])[0]
                r["S7 the reopened cell has the history"] = bool(await pg.until(f"{dialog(b)}.includes('S3-answer')", 15))
                # S9 a subprocess: start, monitor, status, tail, stop — the calls scripted
                await asyncio.sleep(1.5)
                ai = next((x.get("backend_id") for x in rig.cells() if x.get("id") == b), None)
                info["S9_ai"] = ai
                # the ticker: this interpreter, its pid in a file (to find it after the stop)
                script = ("import os, time; open('e75tick.pid', 'w').write(str(os.getpid())); "
                          "[(print('E75TICK', i, flush=True), time.sleep(1)) for i in range(1, 7)]; time.sleep(60)")
                st = {"step": 0, "job": None, "delivered": False, "tail": ""}

                def call(verb: str, **f) -> str:
                    fields = "".join(f"<{k}>{v}</{k}>\n" for k, v in f.items())
                    return f'<call name="send">\n<target_id>svc</target_id>\n<type>{verb}</type>\n{fields}</call>'

                def ticker(body: dict) -> str | None:
                    last = json.dumps(body.get("messages", [])[-1:])
                    j = re.search(r"<job_id>(\d+)</job_id>", last)  # a reply comes back as tags
                    if st["step"] == 0 and "E75-run-ticker" in last:
                        st["step"] = 1
                        return call("start", command=PY, args=f"<item>-c</item><item>{script}</item>")
                    if st["step"] == 1 and j:
                        st["job"], st["step"] = int(j.group(1)), 2
                        return call("monitor", job_id=st["job"], notify=ai, match="E75TICK 2")
                    if st["step"] == 2:
                        st["step"] = 3
                        return 'Watching the ticker.\n<call name="done"></call>'
                    if st["step"] == 3 and "E75TICK 2" in last:
                        st["delivered"] = True
                    if st["step"] == 3 and ("E75TICK 2" in last or "E75-continue" in last):
                        st["step"] = 4
                        return call("status", job_id=st["job"])
                    if st["step"] == 4:
                        st["step"] = 5
                        return call("tail", job_id=st["job"], lines=5)
                    if st["step"] == 5:
                        st["tail"], st["step"] = last[-600:], 6
                        return call("stop", job_id=st["job"])
                    if st["step"] == 6:
                        st["step"] = 7
                        return "S9-done: the ticker is stopped."
                    return None
                mock.responder = ticker
                await ready(pg, b)
                sys.path.insert(0, str(Path(__file__).resolve().parents[6] / "e2e" / "runners"))
                from canvas import host_call
                await send(pg, b, "E75-run-ticker please")
                # the monitor's delivery wakes the agent at "tick 2" — or, refused, the
                # human nudges it on after 6 s
                await pg.until("false", 0.1)
                t9 = time.time()
                while time.time() - t9 < 8 and not st["delivered"]:
                    await asyncio.sleep(0.5)
                if not st["delivered"]:
                    await send(pg, b, "E75-continue")
                # the chain a delivery wakes is the exec agent's, not the person's thread:
                # its end shows in the script's state, not in the chat
                t9 = time.time()
                while time.time() - t9 < 20 and st["step"] < 7:
                    await asyncio.sleep(0.3)
                done9 = st["step"] == 7
                mock.responder = None
                await asyncio.sleep(1)
                pidf = rig.proj / "e75tick.pid"
                tick_pid = int(pidf.read_text()) if pidf.exists() else None
                alive = [tick_pid] if tick_pid is not None and oslayer.pid_alive(tick_pid) else []
                info["S9_state"] = {k: v for k, v in st.items() if k != "tail"}
                for p in alive:
                    oslayer.kill_group(p)  # the test's own ticker, whatever happened
                info["S9_tail_has_ticks"] = "E75TICK" in st["tail"]
                info["S9_left_alive"] = alive
                r["S9 start, status, tail, stop a subprocess"] = bool(done9) and info["S9_tail_has_ticks"] and not alive
                # KNOWN (2026-10-11): the exec agent's delivery is refused — the canvas AI's
                # callable_by does not list it, and exec_bridge drops the refusal silently
                r["S9 the monitor's delivery wakes the agent"] = st["delivered"]
                # ...and a refused delivery is no longer silent: the monitor is listed DEAD with the refusal
                mons = await host_call(rig.port, "svc", {"type": "monitors"})
                info["S9_monitors"] = mons
                ms = mons.get("monitors") if isinstance(mons, dict) else None
                r["S9 a refused delivery shows as a dead monitor"] = st["delivered"] or bool(ms) and any(m.get("status") == "dead" and m.get("error") for m in ms)
                # S10 an inquiry to the person: a card, a click, the answer on the agent's thread
                inq = {"step": 0, "answer_turn": ""}

                def asker(body: dict) -> str | None:
                    last = json.dumps(body.get("messages", [])[-1:])
                    if inq["step"] == 0 and "E75-ask-me" in last:
                        inq["step"] = 1
                        return ('<call name="send">\n<target_id>user</target_id>\n<type>inquiry</type>\n<text>Which colour for the button?</text>\n'
                                '<kind>single</kind>\n<options><item>red</item><item>blue</item></options>\n</call>\n<call name="await"></call>')
                    if inq["step"] == 1 and "user inquiry" in last and "<answer>" in last:
                        inq["step"], inq["answer_turn"] = 2, last[-300:]
                        m = re.search(r"<answer>([^<]*)</answer>", last.replace("\\u003c", "<").replace("\\u003e", ">"))
                        return f"S10-done: {m.group(1) if m else '?'}"
                    return None
                mock.responder = asker
                await send(pg, b, "E75-ask-me which colour")
                card = await pg.until(f"!!document.querySelector('.agent-frame[data-id=\"{b}\"] .c-inquiry')", 12)
                info["S10_card_text"] = await pg.js(f"document.querySelector('.agent-frame[data-id=\"{b}\"] .c-inquiry .c-inq-text')?.textContent")
                info["S10_buttons"] = await pg.js(f"[...document.querySelectorAll('.agent-frame[data-id=\"{b}\"] .c-inquiry .c-inq-row button')].map(b=>b.textContent)")
                # the card as the person sees it (artifact: s10_inquiry.png)
                box = await pg.rect(f'.agent-frame[data-id="{b}"]')
                if box:
                    import base64
                    shot = await pg.cdp("Page.captureScreenshot", format="png", clip={"x": box["x"], "y": box["y"], "width": box["w"], "height": box["h"], "scale": 1})
                    (rig.art / "s10_inquiry.png").write_bytes(base64.b64decode(shot["data"]))
                await pg.js(f"[...document.querySelectorAll('.agent-frame[data-id=\"{b}\"] .c-inquiry .c-inq-row button')].find(x=>x.textContent==='blue')?.click()")
                answered = await pg.until(f"{dialog(b)}.includes('S10-done: blue')", 12)
                info["S10_status"] = await pg.js(f"document.querySelector('.agent-frame[data-id=\"{b}\"] .c-inquiry .c-inq-status')?.textContent")
                info["S10_answer_turn"] = inq["answer_turn"]
                mock.responder = None
                r["S10 an inquiry shows as a card, and the click answers it"] = bool(card) and info["S10_buttons"] == ["red", "blue"] and bool(answered)
                # S11 clear: a new thread — the next request no longer carries the old turns
                await pg.js(f"document.querySelector('.agent-frame[data-id=\"{b}\"] .c-clear').click()")
                cleared = await pg.until(f"{dialog(b)}.includes('cleared: a new thread')", 8)
                mock.clear()
                mock.reply("S11-fresh")
                await send(pg, b, "after the clear")
                await pg.until(f"{dialog(b)}.includes('S11-fresh')", 10)
                # the request that answered THIS message (a reminder of the turn before may come first)
                body11 = json.dumps(next((t.get("body", {}).get("messages", []) for t in mock.turns() if "after the clear" in last_text(t.get("body", {}))), []))
                info["S11_old_in_request"] = "S3-answer" in body11
                info["S11_stats"] = await pg.js(f"document.querySelector('.agent-frame[data-id=\"{b}\"] .c-stats')?.textContent")
                r["S11 clear starts a new thread"] = bool(cleared) and not info["S11_old_in_request"]
                # S12 a LIVE panel, as the docs teach it: spawn the python_agent starter on the
                # canvas's host node, place it as a host cell; a `count` call emits its event
                lp = {"step": 0, "id": None}
                view12 = f"{rig.page_guid()}|view_root"  # read again: the reload in S6 may give the page a new identity

                def live(body: dict) -> str | None:
                    last = json.dumps(body.get("messages", [])[-1:])
                    lp.setdefault("seen", []).append((lp["step"], last[-260:]))
                    if lp["step"] == 0 and "E75-live-panel" in last:
                        lp["step"] = 1
                        return '<call name="send">\n<target_id>canvas_root</target_id>\n<type>spawn</type>\n<bundle>python_agent</bundle>\n</call>'
                    m = re.search(r"<id>(py_[0-9a-f]+_\d+)</id>", last.replace("\\u003c", "<").replace("\\u003e", ">"))
                    if lp["step"] == 1 and m:
                        lp["id"], lp["step"] = m.group(1), 2
                        return (f'<call name="send">\n<target_id>{view12}</target_id>\n<type>place</type>\n'
                                f'<backend_id>{lp["id"]}</backend_id>\n<x>1300</x>\n<y>100</y>\n</call>')
                    if lp["step"] == 2 and "place" in last:
                        lp["place_reply"] = last[-500:]
                        lp["step"] = 3
                        return 'S12-done: the live panel is placed.\n<call name="done"></call>'
                    return None
                mock.responder = live
                await send(pg, b, "E75-live-panel please")
                t12 = time.time()
                while time.time() - t12 < 20 and lp["step"] < 3:
                    await asyncio.sleep(0.3)
                mock.responder = None
                pid = lp["id"]
                info["S12_agent"] = pid
                info["S12_place_reply"] = lp.get("place_reply")
                placed = bool(pid) and bool(await pg.until(f"[...document.querySelectorAll('.agent-frame')].some(f=>f.dataset.side==='host' && f.querySelector('iframe'))", 10))
                frames12 = await pg.js("[...document.querySelectorAll('.agent-frame')].map(f=>[f.dataset.id, f.dataset.side, !!f.querySelector('iframe')])")
                info["S12_frames"] = frames12
                emitted = (await host_call(rig.port, pid, {"type": "count"})) if pid else {}
                info["S12_count"] = emitted
                r["S12 a live panel as the docs teach it: spawned, placed, emitting"] = lp["step"] == 3 and placed and isinstance(emitted, dict) and emitted.get("count") == 1
                # S13 a call with its `<` dropped (vLLM's reasoning parser loses it): the
                # call checker REPAIRS it — the call runs, the row shows it whole, no notice
                mock.clear()
                mock.reply('call name="await"></call>', "S13-recovered")
                await send(pg, b, "E75-malformed")
                recovered0 = await pg.until(f"{dialog(b)}.includes('S13-recovered')", 15)
                shown13 = await pg.js(f"!!document.querySelector('.agent-frame[data-id=\"{b}\"] .c-malformed')")
                info["S13_notice"] = await pg.js(f"document.querySelector('.agent-frame[data-id=\"{b}\"] .c-malformed')?.textContent?.slice(0,200)")
                recovered = await pg.until(f"{dialog(b)}.includes('S13-recovered')", 10)
                ai13 = next((x.get("backend_id") for x in rig.cells() if x.get("id") == b), None)
                h13 = await host_call(rig.port, ai13, {"type": "history"}) if ai13 else {}
                info["S13_rows"] = [(x.get("id"), x.get("kind"), x.get("role"), (x.get("head") or "")[:120]) for x in (h13.get("rows") or [])[-6:]] if isinstance(h13, dict) else h13
                info["S12_place"] = lp.get("place_reply")
                info["S12_seen"] = lp.get("seen")
                rows13 = info.get("S13_rows") or []
                repaired = any(x[2] == "assistant" and str(x[3]).startswith('<call name="await"') for x in rows13 if isinstance(x, (list, tuple)))
                r["S13 a dropped-< call is repaired and runs"] = repaired and not shown13 and bool(recovered)
                # S14 thinking: a collapsible block that stays, collapsed, with its text
                mock.clear()
                mock.reply("<think>E75 a deep thought about colours</think>S14-answer")
                await send(pg, b, "E75-think")
                await pg.until(f"{dialog(b)}.includes('S14-answer')", 12)
                await asyncio.sleep(0.5)
                info["S14_think"] = await pg.js(f"(()=>{{const d=[...document.querySelectorAll('.agent-frame[data-id=\"{b}\"] details.c-think')].pop(); return d ? [d.querySelector('summary')?.textContent, d.open, d.querySelector('.c-think-body')?.textContent] : null}})()")
                t14 = info["S14_think"] or [None, None, None]
                # S16 the thought is KEPT on its row: after a reload it is back, collapsed
                await asyncio.sleep(1.0)
                await pg.open(rig.url)
                await pg.until(f"!!document.querySelector('.agent-frame[data-id=\"{b}\"] .chat-view[data-ready]')", 15)
                info["S16_think"] = await pg.js(f"(()=>{{const d=[...document.querySelectorAll('.agent-frame[data-id=\"{b}\"] details.c-think')].pop(); return d ? [d.querySelector('summary')?.textContent, d.open, d.querySelector('.c-think-body')?.textContent] : null}})()")
                t16 = info["S16_think"] or [None, None, None]
                r["S16 a kept thought is back after a reload, collapsed"] = t16[1] is False and "deep thought" in (t16[2] or "")
                r["S14 thinking stays as a collapsed block with its text"] = bool(t14[0]) and t14[0].startswith("thought for") and t14[1] is False and "deep thought" in (t14[2] or "")
                # S15 the footer: the backend's REPORTED usage — tok/s and the prompt of the window
                mock.clear()
                mock.completion_tokens = 120
                mock.stall = 0.5  # a real generation time (an instant one reports gen_seconds 0)
                mock.reply("S15-usage")
                await send(pg, b, "E75-usage")
                await pg.until(f"{dialog(b)}.includes('S15-usage')", 10)
                await asyncio.sleep(1.0)
                info["S15_stats"] = await pg.js(f"document.querySelector('.agent-frame[data-id=\"{b}\"] .c-stats')?.textContent")
                mock.completion_tokens = None
                mock.stall = 0.0
                st15 = info["S15_stats"] or ""
                r["S15 the footer shows the reported tok/s and context"] = bool(re.match(r"^\d+ tok/s · [\d.]+k? / 262k ctx$", st15))
                if os.environ.get("E75_NO_FOLD"):
                    return
                # S8 one fold at a small window
                long = "lorem ipsum dolor sit amet, consectetur adipiscing elit. " * 120  # ~2.5k tokens a reply
                # the model's SIDE calls, each answered in its own shape so neither takes a
                # turn's queued reply: the fold's SUMMARY (ai_core's fold prompt), and the
                # `context_recall` a fold batch makes due (its rebuild prompt, JSON only)
                def side_call(body):
                    if "FOLD this finished piece of work" in json.dumps(body):
                        return "Filler turns about lorem ipsum\n\nASKED: filler turns\nDONE: answered each\nBINDING: none\nERRORS: none\nOPEN: none"
                    if last_text(body).startswith("You are rebuilding your own working context"):
                        return '{"expand": [], "why": "the filler is folded"}'
                    return None

                small.responder = side_call
                # an AI on the `small` set-up: the default's cell, then its ⚙ →
                # `‹ other set-ups` → the saved `small` rebinds it (no menu tile)
                # on EMPTY canvas: the reopened cell is the kept dialog's own, back at its
                # old place (#133), which may cover a fixed point
                ex, ey = await pg.js(EMPTY_POINT)
                await spot(pg, ex, ey, "AI agent")
                await pg.until(f"{CHATS}.length === 2", 15)
                c = [x for x in await pg.js(CHATS) if x != b][0]
                await ready(pg, c)
                await other_setups(pg, c)
                await pg.js("[...document.querySelectorAll('.wiz .saved button')].find(b=>b.querySelector('.name')?.textContent==='small')?.click()")
                await rebound(pg, c, 15)
                await ready(pg, c)
                for k in range(8):
                    small.reply(f"S8-turn-{k} " + long)
                    await send(pg, c, f"turn {k} " + long[:300])
                    await pg.until(f"{dialog(c)}.includes('S8-turn-{k}')", 10)
                    if await pg.js(f"{dialog(c)}.includes('context folded')"):
                        break
                info["S8_turns_sent"] = k + 1
                folded = await pg.js(f"{dialog(c)}.includes('context folded')")
                small.reply("S8-after-fold")
                await send(pg, c, "still there?")
                r["S8 folds once and still answers"] = bool(folded) and bool(await pg.until(f"{dialog(c)}.includes('S8-after-fold')", 10))
                info["S8_tail"] = (await pg.js(dialog(c)))[-300:]
                info["console"] = [x for x in pg.logs if "WebGL" not in x][:6]

        asyncio.run(run())
    finally:
        mock.stop()
        small.stop()
        rig.stop()
    rig.summary["checks"] = r
    info["nudges_answered_done"] = [mock.nudges, small.nudges]
    rig.summary["info"] = info
    print(json.dumps(info, indent=1, default=str)[:3000])
    failed = [k for k, v in r.items() if not v]
    return verdict(*rig.finish(not failed, f"{len(r) - len(failed)}/{len(r)} pass" + ("" if not failed else f" — failed: {'; '.join(failed)}")))


if __name__ == "__main__":
    sys.exit(0 if main() else 1)
