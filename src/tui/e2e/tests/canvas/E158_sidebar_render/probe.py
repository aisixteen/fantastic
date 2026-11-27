"""E158 sidebar_render — the sidebar view as drawn in the canvas's tree cell (#608, #698, #714).

The cell's iframe is null-origin: its document is read through CDP (`frame_eval`).

Part 8 (#714) drives two AI chats over a scripted model (the parity OpenAI chat stub, no GPU)
through the design's agent states on their real paths, and checks each chat row's badge in
the tree; each state's tree cell and row are screenshot next to the mock's "Agent state"."""

from __future__ import annotations

import asyncio
import base64
import json
import os
import re
import secrets
import socket
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[2]))
from _lib.canvas_live import Rig, verdict  # noqa: E402
from _lib.dev_surface import frame_eval  # noqa: E402

# the tree cell: the cell whose view draws the tree (the only cell on a fresh canvas)
TREE = "(()=>{const f=[...document.querySelectorAll('.agent-frame')].find(f=>f.querySelector('iframe')); return f ? f.dataset.id : null;})()"
CELL_IFRAME = ".agent-frame iframe"
TREE_AT = {"id": None}  # the tree cell's id, once found

E2E = Path(__file__).resolve().parents[6] / "e2e"
MOCK = Path(os.environ.get("TEAM_MOCK", "/Volumes/Warehouse/tmp/wf/s714/team/design/mock/states.html"))
LIGHT = Path(__file__).resolve().parents[4] / "web" / "page" / "view_skins" / "light.css"
SHOTS = Path(os.environ.get("E158_SHOTS", "")) if os.environ.get("E158_SHOTS") else None
# the local-time head each message opens with (#653): the scripted model reads
# the message past it
TS_HEAD = re.compile(r"^\[\d{4}-\d\d-\d\d \d\d:\d\d [+-]\d\d:\d\d\]\n")
DONE = '<call name="done"></call>'
ASK = ("<call name=\"send\"><routing><target_id>user</target_id></routing><payload><type>inquiry</type>"
       "<text>Which colour?</text><kind>single</kind><options><item>red</item><item>blue</item></options></payload></call>")
A, B = "c714a", "c714b"  # the two chats: A drives the states, B is the work A waits on
# A hands B a slow message and ends its answer (#795: `done`, B's answer wakes it), B thinking
HANDOFF = (f"<call name=\"send\"><routing><target_id>{B}</target_id></routing><payload><type>send</type><text>S_SLOWB</text></payload></call>"
           "<call name=\"done\"></call>")


def responder(req: dict, _raw: bytes):
    """The scripted model: the newest message's marker picks the answer."""
    msgs = req.get("messages") or []
    last = msgs[-1].get("content") if msgs else ""
    last = TS_HEAD.sub("", last) if isinstance(last, str) else json.dumps(last)
    if last.startswith("[reply to"):
        return "noted " + DONE
    if "S_SLOWB" in last:
        return {"stall": 8, "text": "slow b " + DONE}
    if "S_SLOW" in last:
        return {"stall": 6, "text": "slow " + DONE}
    if "S_WRITE" in last:
        return {"stall_mid": 6, "text": "line one\nthe rest " + DONE}
    # the link retry re-sends the same request: it dies again
    if "S_RETRY" in last:
        return {"cut": True, "text": "a stream that dies"}
    # an empty answer is re-asked inside its turn (#795): it stays empty
    if "S_DEAD" in last or last.startswith("[no call was dispatched]"):
        return ""
    if "S_HANDOFF" in last:
        return HANDOFF
    if "S_ASK" in last:
        return ASK
    return "ok " + DONE


def chat_record(cid: str, stub_url: str, can_call: dict) -> dict:
    """A vllm chat on the stub. `transport_retries` 1: a dropped stream is retried once
    (Working, retrying), then the call fails (Failed), so the badge stays to be looked at."""
    return {"id": cid, "handler_module": "vllm_backend.tools", "endpoint": stub_url, "model": "stub",
            "context_window": 32768, "max_tokens": 4096, "kv_slots": 4, "kv_overflow": "evict",
            "transport_retries": 1, "can_call": can_call}


class _Rec:
    def __init__(self, root: Path) -> None:
        self.root = root

    def raw(self, rel: str, data: bytes) -> None:
        p = self.root / rel
        p.parent.mkdir(parents=True, exist_ok=True)
        p.write_bytes(data)

    def json(self, rel: str, obj) -> None:
        p = self.root / f"{rel}.json"
        p.parent.mkdir(parents=True, exist_ok=True)
        p.write_text(json.dumps(obj, default=str))


async def fire(port: int, target: str, payload: dict, secs: float = 60) -> dict:
    """A host call through `user` (as a canvas tab makes one) that may take a whole call to answer."""
    import websockets

    frame = {"type": "call", "target": "user", "payload": {"type": "forward", "target": target, "payload": payload},
             "id": secrets.token_hex(6)}
    async with websockets.connect(f"ws://127.0.0.1:{port}/user/ws", origin=f"http://127.0.0.1:{port}") as ws:
        await ws.send(json.dumps(frame))
        while True:
            m = json.loads(await asyncio.wait_for(ws.recv(), secs))
            if m.get("type") in ("reply", "error") and m.get("id") == frame["id"]:
                return m.get("data", m)


# every state a chat row has drawn, in order (a MutationObserver in the view's frame; kept on
# the document, not in the probe's isolated world, which is new on every evaluation)
WATCH_ROWS = """(()=>{const t=document.getElementById('tree'); if(!t) return false; if(document.documentElement.dataset.seen!==undefined) return true;
document.documentElement.dataset.seen='[]';
const note=()=>{const seen=JSON.parse(document.documentElement.dataset.seen);
 for(const id of %s){const r=t.querySelector('[data-id="'+id+'"]'); if(!r) continue; const b=r.querySelector('.badge'), s=r.querySelector('.sub');
  const now=id+':'+(b?b.className.replace('badge','').trim():'none')+':'+(s?s.textContent:'');
  const last=seen.filter(x=>x.startsWith(id+':')).pop(); if(last!==now) seen.push(now);
  if(b&&b.classList.contains('stalled')&&document.documentElement.dataset.stall===undefined){const g=r.querySelector('.gl'), o=getComputedStyle(g,'::before'), p=document.createElement('i'); p.style.color='color-mix(in srgb,var(--fc-color-status-warn) 40%%,var(--fc-color-status-error))'; document.body.appendChild(p);
   document.documentElement.dataset.stall=JSON.stringify({gl:g.className, ol_img:o.backgroundImage, bg:getComputedStyle(b).backgroundColor, sub:s?getComputedStyle(s).color:null, stall:getComputedStyle(p).color}); p.remove();}}
 document.documentElement.dataset.seen=JSON.stringify(seen);};
new MutationObserver(note).observe(t,{childList:true,subtree:true,characterData:true,attributes:true}); note(); return true;})()"""

# what a chat row draws now: its badge (class, glyph, colours), its word, the face's filter
ROW_STATE = """(()=>{const r=document.querySelector('#tree [data-id="%s"]'); if(!r) return null;
const b=r.querySelector('.gl .badge'), s=r.querySelector('.sub'), f=r.querySelector('.gl .face'), root=getComputedStyle(document.documentElement);
const cs=b?getComputedStyle(b):null; const col=e=>{const p=document.createElement('i'); p.style.color=e; document.body.appendChild(p); const c=getComputedStyle(p).color; p.remove(); return c;}; const tok=n=>col('var('+n+')');
const g=r.querySelector('.gl'), o=getComputedStyle(g,'::before');
return {kind: b?b.className.replace('badge','').trim():null, glyph: b?b.textContent:null, shown: !!cs && cs.display!=='none',
 bg: cs?cs.backgroundColor:null, fg: cs?cs.color:null, ring: cs?cs.boxShadow:null, border: cs?cs.borderTopColor+'|'+cs.borderRightColor:null, anim: cs?cs.animationName:null,
 sub: s?s.textContent:null, sub_fg: s?getComputedStyle(s).color:null, face: f?getComputedStyle(f).filter:null,
 gl: g.className, ol_bg: o.backgroundColor, ol_img: o.backgroundImage, ol_anim: o.animationName, ol_content: o.content, ol_inset: o.top + ' ' + o.left, ol_pad: o.paddingTop,
 tok: {warn: tok('--fc-color-status-warn'), warn_on: tok('--fc-color-status-warn-on'), error: tok('--fc-color-status-error'), ok: tok('--fc-color-status-ok'), ok_on: tok('--fc-color-status-ok-on'),
       subtle: tok('--fc-color-text-subtle'), accent: tok('--fc-color-accent-base'), accent_on: tok('--fc-color-accent-on'), raised: tok('--fc-color-surface-raised'),
       stall: col('color-mix(in srgb,var(--fc-color-status-warn) 40%%,var(--fc-color-status-error))')}};})()"""


async def side(pg, expr: str):
    """`expr` (JS; the view's own document) evaluated in the sidebar view's frame; None while it has not drawn."""
    wrapped = f"(()=>{{if(!document.getElementById('tree')) return null; return {{v:({expr})}};}})()"
    hits = [h for h in await frame_eval(pg, wrapped) if isinstance(h, dict)]
    return hits[0]["v"] if hits else None


async def until(pg, expr: str, secs: float = 20) -> bool:
    for _ in range(int(secs * 5)):
        if await side(pg, expr):
            return True
        await asyncio.sleep(0.2)
    return False


async def click(pg, sel: str) -> bool:
    """A real mouse click at the middle of `sel` (an element of the view), through the column's iframe."""
    box = await side(pg, f"(()=>{{const e=document.querySelector({json.dumps(sel)}); if(!e) return null; const r=e.getBoundingClientRect(); return [r.x+r.width/2, r.y+r.height/2, window.innerWidth];}})()")
    frame = await pg.rect(f".agent-frame[data-id={TREE_AT['id']}] iframe" if TREE_AT["id"] else CELL_IFRAME)
    if not box or not frame:
        return False
    # a cell is drawn at the canvas's zoom: the frame's own pixels are scaled to the page's
    k = frame["w"] / box[2] if box[2] else 1
    x, y = frame["x"] + box[0] * k, frame["y"] + box[1] * k
    await pg.cdp("Input.dispatchMouseEvent", type="mouseMoved", x=x, y=y)
    await pg.mouse("mousePressed", x, y)
    await pg.mouse("mouseReleased", x, y)
    return True


async def shot(pg, name: str, info: dict, light: bool = False) -> None:
    """The tree cell, and the chat rows at 3x, as png files in E158_SHOTS (when set). `light`:
    the rows once more in the light view skin over the light page colour, for the mock's light
    cells (a picture only: the canvas mounts its views dark; the skin goes back after)."""
    if SHOTS is None:
        return
    await _shot(pg, name, info)
    if light:
        await side(pg, "(()=>{const s=document.getElementById('fantastic-skin'); document.documentElement.dataset.dark=s.textContent; s.textContent=%s;"
                       " document.body.style.background='var(--fc-color-page-bg)'; return true;})()" % json.dumps(LIGHT.read_text()))
        await asyncio.sleep(0.2)
        await _shot(pg, name + "-light", info, cell=False)
        await side(pg, "(()=>{const s=document.getElementById('fantastic-skin'); s.textContent=document.documentElement.dataset.dark; document.body.style.background=''; return true;})()")


async def _shot(pg, name: str, info: dict, cell: bool = True) -> None:
    SHOTS.mkdir(parents=True, exist_ok=True)
    frame = await pg.rect(f".agent-frame[data-id={TREE_AT['id']}] iframe")
    await side(pg, f"(()=>{{document.querySelector('#tree [data-id=\"{A}\"]')?.scrollIntoView({{block:'center'}}); return true;}})()")
    await asyncio.sleep(0.15)
    rows = await side(pg, f"(()=>{{const a=document.querySelector('#tree [data-id=\"{A}\"]'), b=document.querySelector('#tree [data-id=\"{B}\"]'); if(!a) return null;"
                          " const ra=a.getBoundingClientRect(), rb=(b||a).getBoundingClientRect(); return [Math.min(ra.x,rb.x), Math.min(ra.y,rb.y), Math.max(ra.right,rb.right), Math.max(ra.bottom,rb.bottom), window.innerWidth];})()")
    out = []
    box = await pg.rect(f".agent-frame[data-id={TREE_AT['id']}]") if cell else None
    if box:
        png = await pg.cdp("Page.captureScreenshot", format="png", clip={"x": box["x"], "y": box["y"], "width": box["w"], "height": box["h"], "scale": 1})
        (SHOTS / f"tree-{name}.png").write_bytes(base64.b64decode(png["data"]))
        out.append(str(SHOTS / f"tree-{name}.png"))
    if frame and rows:
        k = frame["w"] / rows[4] if rows[4] else 1
        x, y = frame["x"] + rows[0] * k, frame["y"] + rows[1] * k
        png = await pg.cdp("Page.captureScreenshot", format="png", clip={"x": x, "y": y, "width": (rows[2] - rows[0]) * k, "height": (rows[3] - rows[1]) * k, "scale": 3})
        (SHOTS / f"rows-{name}.png").write_bytes(base64.b64decode(png["data"]))
        out.append(str(SHOTS / f"rows-{name}.png"))
    info.setdefault("shots", []).extend(out)


async def mock_shots(pg, info: dict) -> None:
    """The mock's "Agent state" section, light and dark, at 2x (when E158_SHOTS is set)."""
    if SHOTS is None or not MOCK.exists():
        info["mock_shots"] = "skipped: no E158_SHOTS or no mock at " + str(MOCK)
        return
    await pg.cdp("Page.navigate", url=MOCK.resolve().as_uri())
    await pg.until("document.readyState==='complete' && [...document.images].every(i=>i.complete)", 15)
    for i, theme in enumerate(("light", "dark")):
        box = await pg.js(f"(()=>{{const g=[...document.querySelector('section').querySelectorAll(':scope > div[data-theme]')][{i}]; const r=g.getBoundingClientRect(); return {{x:r.x+scrollX,y:r.y+scrollY,w:r.width,h:r.height}};}})()")
        if not box:
            continue
        png = await pg.cdp("Page.captureScreenshot", format="png", captureBeyondViewport=True,
                           clip={"x": box["x"], "y": box["y"], "width": box["w"], "height": box["h"], "scale": 2})
        (SHOTS / f"mock-agent-state-{theme}.png").write_bytes(base64.b64decode(png["data"]))
        info.setdefault("shots", []).append(str(SHOTS / f"mock-agent-state-{theme}.png"))


async def states(pg, r: dict, info: dict) -> None:
    """Part 8 (#714): the design's agent states on two scripted chats, each drawn as its badge."""
    rig = pg.rig
    port = rig.port
    from canvas import host_call

    await side(pg, "(()=>{const i=document.getElementById('filter'); i.value=''; i.dispatchEvent(new Event('input')); return true})()")
    for sw in ("#full", "#sw"):
        if await side(pg, f"document.querySelector('{sw}')?.getAttribute('aria-checked')") == "true":
            await click(pg, sw)
    # the chats, made as the canvas makes every AI (`canvas_root create`); A may hand B work
    made = [await host_call(port, "canvas_root", {"type": "create", "record": chat_record(B, rig.stub_url, {"user": ["inquiry", "inquiries"]})}),
            await host_call(port, "canvas_root", {"type": "create", "record": chat_record(A, rig.stub_url, {"user": ["inquiry", "inquiries"], B: ["send"]})})]
    info["made"] = made
    row = f"#tree [data-id=\"{A}\"]"
    r["8a the two chats are rows of the tree, at rest: no badge, no word"] = await until(pg, f"!!document.querySelector('{row}') && !!document.querySelector('#tree [data-id=\"{B}\"]') && getComputedStyle(document.querySelector('{row} .badge')).display==='none' && !document.querySelector('{row} .sub') && !document.querySelector('{row} .gl').classList.contains('ol') && getComputedStyle(document.querySelector('{row} .gl'),'::before').content==='none'", 30)
    await side(pg, WATCH_ROWS % json.dumps([A, B]))
    await shot(pg, "0-idle", info)

    async def state(expr: str, secs: float = 20) -> bool:
        return await until(pg, f"(()=>{{const r=document.querySelector('{row}'); if(!r) return false; const b=r.querySelector('.gl .badge'), s=r.querySelector('.sub'); return {expr};}})()", secs)

    def look(x: dict | None, kind: str, glyph: str, bg: str, fg: str, sub: str) -> bool:
        return bool(x) and x["kind"] == kind and x["glyph"] == glyph and x["shown"] and x["bg"] == x["tok"][bg] and x["fg"] == x["tok"][fg] \
            and x["sub"] == sub and x["ring"].startswith(x["tok"]["raised"]) and x["face"] == "none" and ringed(x, bg)

    def ringed(x: dict | None, colour: str) -> bool:
        """The outline round the tile: a plain 1.5px ring 3px outside it, in the state's colour."""
        return bool(x) and "ol" in x["gl"].split() and "ray" not in x["gl"].split() and "dash" not in x["gl"].split() \
            and x["ol_bg"] == x["tok"][colour] and x["ol_img"] == "none" and x["ol_inset"] == "-3px -3px" and x["ol_pad"] == "1.5px"

    async def seen() -> list:
        return json.loads(await side(pg, "document.documentElement.dataset.seen") or "[]")

    async def ends(after: int, *marks: str, secs: float = 30) -> bool:
        """Until A's drawn states, after the first `after`, hold `marks` in order (kind:word)."""
        for _ in range(int(secs * 5)):
            got = [x.split(":", 1)[1] for x in (await seen())[after:] if x.startswith(A + ":")]
            i = 0
            for g in got:
                if i < len(marks) and g == marks[i]:
                    i += 1
            if i == len(marks):
                return True
            await asyncio.sleep(0.2)
        return False

    async def now(key: str) -> dict | None:
        x = await side(pg, ROW_STATE % A)
        info[key] = x
        return x

    # Working: thinking while the model has not answered; a second message meanwhile is stored
    # and read by the next pass
    slow = asyncio.create_task(fire(port, A, {"type": "send", "text": "S_SLOW"}))
    r["8b Working: the spinner, the word thinking"] = await state("!!b && b.classList.contains('active') && s && s.textContent==='thinking'")
    x = await now("thinking")
    r["8b2 the spinner is the accent ring that spins (no glyph)"] = bool(x) and x["glyph"] == "" and x["anim"] == "spin" and x["border"].endswith(x["tok"]["accent"]) and x["ring"].startswith(x["tok"]["raised"])
    r["8b3 Working's outline: a ray running round the tile (accent over the faint line)"] = bool(x) and x["gl"].split()[-2:] == ["ol", "ray"] \
        and x["ol_anim"] == "ray" and x["ol_img"].startswith("conic-gradient") and x["tok"]["accent"] in x["ol_img"]
    await shot(pg, "1-working-thinking", info, light=True)
    second = asyncio.create_task(fire(port, A, {"type": "send", "text": "S_AGAIN"}))
    r["8c Working: a message sent while a pass runs leaves the tile Working (read by the next pass)"] = await state("!!b && b.classList.contains('active')")
    await shot(pg, "2-working-again", info)
    info["slow"] = await slow
    info["again"] = await second

    # Done: a call that completed keeps the check until the next call
    r["8d Done: a call that completed, the check on ok"] = await ends(0, "active:thinking", "done:done")
    await asyncio.sleep(1)
    r["8d2 kept after the call: the check on ok, the word done"] = look(await now("done"), "done", "\u2713", "ok", "ok_on", "done")
    await shot(pg, "3-done", info, light=True)

    # Working: writing while the answer streams
    n = len(await seen())
    write = asyncio.create_task(fire(port, A, {"type": "send", "text": "S_WRITE"}))
    r["8e Working: the spinner, the word writing (the answer streams)"] = await state("!!b && b.classList.contains('active') && s && s.textContent==='writing'")
    await shot(pg, "4-working-writing", info)
    info["write"] = await write
    await ends(n, "active:writing", "done:done")

    # Working: retrying after a stream that died; then Failed when the retry dies too
    n = len(await seen())
    retry = asyncio.create_task(fire(port, A, {"type": "send", "text": "S_RETRY"}))
    r["8f Working: the spinner, the word retrying (a dropped stream)"] = await state("!!b && b.classList.contains('active') && s && s.textContent==='retrying'")
    await shot(pg, "5-working-retrying", info)
    # the link retry dies too: the call fails, and nothing re-runs it (#795)
    r["8g Failed: the retry died too, the cross on error"] = await ends(n, "active:retrying", "failed:failed", secs=45)
    info["retry"] = await retry
    await asyncio.sleep(1)
    r["8g2 kept after the call: the cross on error, the word failed"] = look(await now("failed"), "failed", "\u00d7", "error", "accent_on", "failed")
    await shot(pg, "6-failed", info, light=True)

    # An empty answer is no call: re-asked inside its turn (Working, retrying), then the
    # harness stops the turn (Failed, #795)
    n = len(await seen())
    dead = asyncio.create_task(fire(port, A, {"type": "send", "text": "S_DEAD"}))
    r["8h an empty answer is re-asked (Working, retrying), then the turn is stopped (Failed)"] = await ends(n, "active:thinking", "active:retrying", "failed:failed")
    info["dead"] = await dead

    # A hands B a slow message and ends its answer with `done` (#795): B thinks meanwhile,
    # A's own call has ended (Done); B's answer wakes A: Working, then Done
    n = len(await seen())
    wait = asyncio.create_task(fire(port, A, {"type": "send", "text": "S_HANDOFF"}))
    r["8i the chat it handed work to is Working, thinking"] = await until(pg, f"document.querySelector('#tree [data-id=\"{B}\"] .sub')?.textContent==='thinking'", 20)
    await shot(pg, "7-b-thinking", info)
    info["handoff"] = await wait
    r["8i2 A ended its answer (Done), then B's answer woke it: Working, then Done"] = await ends(n, "done:done", "active:thinking", "done:done")

    # Asks you: the chat's question to the person; the answer takes the badge away
    n = len(await seen())
    ask = asyncio.create_task(fire(port, A, {"type": "send", "text": "S_ASK"}))
    r["8j Asks you: the ? on warn, the word asks you"] = await state("!!b && b.classList.contains('ask')", 30)
    await asyncio.sleep(0.5)
    r["8j2 drawn as the design: ? on warn"] = look(await now("ask"), "ask", "?", "warn", "warn_on", "asks you")
    await shot(pg, "8-asks-you", info, light=True)
    inq = (await host_call(port, "user", {"type": "inquiries"})).get("inquiries") or []
    info["inquiries"] = inq
    mine = [q for q in inq if q.get("asker") == A]
    if mine:
        info["answered"] = await host_call(port, "user", {"type": "answer_inquiry", "re": mine[0]["re"], "answer": "red"})
    r["8j3 the answer takes the ? away"] = await state("!b || !b.classList.contains('ask')", 30)
    info["ask"] = await ask
    r["8j4 the answer wakes it: Done"] = await ends(n, "ask:asks you", "done:done")

    # Paused: the pause verb; the face greyed. Resume with nothing held: at rest, no badge
    info["pause"] = await host_call(port, A, {"type": "pause"})
    r["8k Paused: II on muted"] = await state("!!b && b.classList.contains('paused')")
    x = await now("paused")
    r["8k2 drawn as the design: II on muted, the face greyed"] = bool(x) and x["kind"] == "paused" and x["glyph"] == "II" and x["bg"] == x["tok"]["subtle"] \
        and x["fg"] == x["tok"]["accent_on"] and x["sub"] == "paused" and "grayscale(1)" in (x["face"] or "") and "opacity(0.6)" in (x["face"] or "") \
        and ringed(x, "subtle")
    await shot(pg, "9-paused", info, light=True)

    # Paused after a restart: the host stops and starts again; the sidebar boots before the
    # chats, so A's own `paused` at its boot (its store says paused) is drawn again
    await pg.cdp("Page.navigate", url="about:blank")
    rig.stop()
    rig.start()
    await pg.open(rig.url)
    r["8k3 after a restart the view draws again"] = await until(pg, f"!!document.querySelector('{row}')", 40)
    await side(pg, WATCH_ROWS % json.dumps([A, B]))
    r["8k4 after a restart: still II on muted, the word paused"] = await state("!!b && b.classList.contains('paused') && s && s.textContent==='paused'")
    x = await now("paused_after_restart")
    r["8k5 drawn as before the restart: II on muted, the face greyed"] = bool(x) and x["kind"] == "paused" and x["glyph"] == "II" and x["bg"] == x["tok"]["subtle"] \
        and x["sub"] == "paused" and "grayscale(1)" in (x["face"] or "") and ringed(x, "subtle")
    await shot(pg, "11-paused-after-restart", info)
    info["resume"] = await host_call(port, A, {"type": "resume"})
    r["8l resume with nothing held: at rest, no badge, no word, the face in colour"] = await state("(!b || getComputedStyle(b).display==='none') && !s && getComputedStyle(r.querySelector('.gl .face')).filter==='none' && !r.querySelector('.gl').classList.contains('ol')")
    await shot(pg, "10-resumed-idle", info)
    info["seen"] = json.loads(await side(pg, "document.documentElement.dataset.seen") or "[]")


def main() -> bool:
    sys.path.insert(0, str(E2E / "parity"))
    from stubs.openai_chat import Handler, OpenAIChatStub  # noqa: E402

    rig = Rig(__file__, live=False)
    rig.build()
    with socket.socket() as so:
        so.bind(("127.0.0.1", 0))
        stub_port = so.getsockname()[1]
    stub = OpenAIChatStub(_Rec(rig.art / "stub"), "stub", stub_port)
    stub.responder = responder
    stub.start(Handler)
    rig.stub_url = stub.base_url()
    rig.make_home(None)
    (rig.proj / "docs").mkdir()
    (rig.proj / "docs" / "a.md").write_text("# a\n")
    (rig.proj / "notes.txt").write_text("notes\n")
    rig.init(tree=True)
    r: dict = {}
    info: dict = {}

    async def run() -> None:
        async with rig.page(1400, 900) as pg:
            r["1a the tree cell is drawn on the fresh canvas and the view has drawn in it"] = bool(await pg.until(f"!!document.querySelector({json.dumps(CELL_IFRAME)})", 30)) and await until(pg, "!!document.querySelector('.rail .k')", 40)
            tree_cell = await pg.js(TREE)
            info["tree_cell"] = TREE_AT["id"] = tree_cell
            r["1a2 it is a cell like any other: at the canvas's left, not full screen"] = bool(tree_cell) and await pg.js(f"(()=>{{const f=document.querySelector('.agent-frame[data-id={tree_cell}]'); const c=document.getElementById('canvas').getBoundingClientRect(); const r=f.getBoundingClientRect(); return !f.classList.contains('full') && r.left - c.left < 200 && r.width < c.width / 2;}})()") is True
            parts = await side(pg, "({"
                "strip: !!document.querySelector('nav[aria-label=Kernels] .k.here'),"
                "head: !!document.querySelector('.head h1'),"
                "agents: !!document.querySelector('nav[aria-label=Agents] #tree[role=tree]'),"
                "filter: !!document.querySelector('input[aria-label=\"Filter agents and files\"]'),"
                "files_switch: document.querySelector('button[role=switch][aria-label=\"Show files in the tree\"]')?.getAttribute('aria-checked'),"
                "waiting_hidden: document.getElementById('wait').hidden === true})")
            info["parts"] = parts
            r["1b kernel strip, header, Agents nav, filter, Files switch (off)"] = bool(parts) and parts["strip"] and parts["head"] and parts["agents"] and parts["filter"] and parts["files_switch"] == "false"
            r["1c the Waiting for you box is hidden while nothing asks"] = bool(parts) and parts["waiting_hidden"]

            # the tree opens compact (AI agents under the root, the rest in the tools group); Full shows the whole kernel tree
            r["1d the tree opens compact (the Full switch off)"] = await side(pg, "document.querySelector('#full')?.getAttribute('aria-checked') === 'false'") is True
            r["1e the Full switch on"] = await click(pg, "#full") and await until(pg, "!!document.querySelector('#tree [data-id=files]') && !!document.querySelector('#tree [data-id=user]')", 20)
            rows = await side(pg, "[...document.querySelectorAll('#tree [role=treeitem]')].map(e => ({id: e.dataset.id || null, path: e.dataset.path || null, pad: e.style.paddingLeft}))")
            info["rows"] = rows
            ids = [x["id"] for x in rows or [] if x["id"]]
            r["2a agent rows are drawn: the root first (depth 0), then user and files"] = bool(rows) and rows[0]["pad"] == "0px" and "user" in ids and "files" in ids
            r["2b no file row while the switch is off"] = bool(rows) and not any(x["path"] for x in rows)

            r["3a the Files switch on"] = await click(pg, "#sw")
            r["3b the project's files are rows inside the tree"] = await until(pg, "!!document.querySelector('#tree [data-path=\"docs\"]') && !!document.querySelector('#tree [data-path=\"notes.txt\"]')", 30)
            below = await side(pg, "(()=>{const f=document.querySelector('#tree [data-id=files]'), d=document.querySelector('#tree [data-path=\"docs\"]'); return !!f && !!d && parseFloat(d.style.paddingLeft) === parseFloat(f.style.paddingLeft) + 14 && !!(f.compareDocumentPosition(d) & Node.DOCUMENT_POSITION_FOLLOWING)})()")
            r["3c they sit one level under the files agent"] = bool(below)
            r["3d a folder opens in place"] = await click(pg, "#tree [data-path=\"docs\"] .tw") and await until(pg, "!!document.querySelector('#tree [data-path=\"docs/a.md\"]')", 20)

            before = await pg.js("document.querySelectorAll('.agent-frame').length")
            r["4a a click on the user agent row"] = await click(pg, "#tree [data-id=user] .main")
            r["4b opens its view: a cell is drawn"] = bool(await pg.until(f"document.querySelectorAll('.agent-frame').length > {before or 0}", 20))
            r["4b2 as a cell, not full screen"] = await pg.js("!document.querySelector('#canvas.full-view')") is True
            r["4c and the row is selected"] = await until(pg, "document.querySelector('#tree [data-id=user]')?.classList.contains('sel')", 10)

            await pg.open(rig.url)
            r["5a after a reload the view draws again"] = await until(pg, "!!document.querySelector('.rail .k')", 40)
            r["5b2 and the Full switch still on"] = await until(pg, "document.querySelector('#full')?.getAttribute('aria-checked') === 'true'", 20)
            r["5b with the Files switch still on"] = await until(pg, "document.querySelector('#sw')?.getAttribute('aria-checked') === 'true'", 20)
            r["5c and the folder still open"] = await until(pg, "!!document.querySelector('#tree [data-path=\"docs/a.md\"]')", 30)

            before = await pg.js("document.querySelectorAll('.agent-frame').length")
            r["5d a file row dragged out of the panel"] = await side(pg, "(()=>{const w=document.querySelector('#tree [data-path=\"docs/a.md\"]'); if(!w||!w.draggable) return false; w.dispatchEvent(new DragEvent('dragend',{bubbles:true,clientX:window.innerWidth+200,clientY:40})); return true})()") is True
            r["5e opens it on the canvas: a cell is drawn"] = bool(await pg.until(f"document.querySelectorAll('.agent-frame').length > {before or 0}", 20))

            r["6a the filter narrows the tree"] = await side(pg, "(()=>{const i=document.getElementById('filter'); i.value='a.md'; i.dispatchEvent(new Event('input')); return true})()") is True
            r["6b to the match and the rows above it"] = await until(pg, "(()=>{const t=[...document.querySelectorAll('#tree [role=treeitem]')]; return t.length >= 2 && t.length < 12 && !!document.querySelector('#tree [data-path=\"docs/a.md\"]') && !document.querySelector('#tree [data-path=\"notes.txt\"]')})()", 10)

            await states(pg, r, info)

            info["page_errors"] = [x for x in pg.logs if "WebGL" not in x][:8]
            r["7 no page error"] = not info["page_errors"]
            await mock_shots(pg, info)

    try:
        asyncio.run(run())
    finally:
        rig.stop()
        stub.stop()
    rig.summary["checks"] = r
    rig.summary["info"] = info
    print(json.dumps(info, indent=1, default=str)[:12000])
    failed = [k for k, v in r.items() if not v]
    return verdict(*rig.finish(not failed, f"{len(r) - len(failed)}/{len(r)} pass" + ("" if not failed else f" — failed: {'; '.join(failed)}")))


if __name__ == "__main__":
    sys.exit(0 if main() else 1)
