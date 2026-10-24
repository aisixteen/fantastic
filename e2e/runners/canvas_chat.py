"""Run the canvas chat-card scenarios (E25 scripted, E26 with a real model) — #339.

    cd src/lib/python && uv run python -u ../../../e2e/runners/canvas_chat.py canvas_chat_cards

A chat cell of the canvas shows a host agent's panel as a CARD, where the AI's
`embed {id}` row sits in the conversation. One host (`fantastic canvas`), ONE
headless Chrome, and the chat's AI: a `vllm_backend` agent whose endpoint is the
scripted mock (E25), or the plug `BACKEND` names (E26; with none, `E2E-SKIP`).
The chat cell is placed on the page's view from the host (`<page guid>|view_root`),
the person types in its input, and every check reads the page's DOM.
"""

from __future__ import annotations

import asyncio
import json
import os
import re
import shutil
import subprocess
import sys
import tempfile
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from canvas import DESKTOP_POINTER, MAC_CHROME, Host, Page, _scenario_path, attach, build_bin, free_port, host_call  # noqa: E402

E2E = next(d for d in Path(__file__).resolve().parents if (d / ".e2e-root").exists())
sys.path.insert(0, str(E2E))
from helpers import verdict  # noqa: E402
from helpers.chrome import Chrome  # noqa: E402

AI = "ai"
CAP = 1200  # CHAT_CARD_MAX_HEIGHT (ai-core `constants.rs`, canvas `chat_card.ts`)

#: the panel the scripted model writes: it sets NO width, defines its height, and redraws on an
#: event of its own agent. `note {text, h}` is how the test makes the agent "emit an update".
def panel_bytes(agent: str, height: int = 300) -> str:
    return (f'<div id="o" style="height:{height}px;font:16px sans-serif;padding:8px">card-start</div>'
            f"<script>fantastic.watch('{agent}',function(e){{var o=document.getElementById('o');"
            "o.textContent=e.text||'';if(e.h)o.style.height=e.h+'px';});</script>")


def create_call(agent: str, height: int = 300) -> str:
    """The model's `create_agent` of a host agent with a `view` record, in the call grammar."""
    return ('Making the card.\n<call name="send">\n<target_id>kernel_state</target_id>\n<type>create_agent</type>\n'
            f"<parent_id>canvas_root</parent_id>\n<handler_module>ram_store.tools</handler_module>\n<id>{agent}</id>\n"
            "<view><kind>html</kind><calls><item>get_html</item><item>watch</item></calls>"
            f"<bytes>{panel_bytes(agent, height)}</bytes></view>\n"
            "<callable_by><user><item>get_html</item><item>watch</item><item>emit</item></user></callable_by>\n</call>")


def embed_call(agent: str) -> str:
    return f'Showing it.\n<call name="send">\n<target_id>{AI}</target_id>\n<type>embed</type>\n<id>{agent}</id>\n</call>'


DONE_TEXT = 'Here it is.\n<call name="done"></call>'

#: what the person asks, in order, and the card the scripted model makes for it
ASKS = [("show me a card", "card_a", 300), ("now a big one", "card_b", 3000), ("another card", "card_c", 300),
        ("one more card", "card_d", 300)]


def responder(req: dict) -> str | None:
    """The scripted model answers by WHAT IT WAS LAST TOLD, not by its turn in a queue (the page
    may add a turn of its own: the overflow notice). A person's ask → create the card; the reply
    to the create → embed it; the reply to the embed, a notice, anything else → done."""
    u = ""
    for m in reversed(req.get("messages") or []):
        if m.get("role") == "user":
            c = m.get("content")
            u = c if isinstance(c, str) else json.dumps(c)
            break
    if "[reply to" in u and "create_agent" in u and (made := re.search(r"<id>(\w+)</id>", u)):
        return embed_call(made.group(1))
    if "[reply to" in u or "[chat card" in u:
        return DONE_TEXT
    for text, agent, height in ASKS:
        if text in u:
            return create_call(agent, height)
    return DONE_TEXT


def view_record(agent: str, height: int = 300) -> dict:
    """The same agent, made by the runner (the `create_agent` fields)."""
    return {"type": "create_agent", "parent_id": "canvas_root", "handler_module": "ram_store.tools", "id": agent,
            "view": {"kind": "html", "calls": ["get_html", "watch"], "bytes": panel_bytes(agent, height)},
            "callable_by": {"user": ["get_html", "watch", "emit"]}}


CARDS_JS = """(()=>[...document.querySelectorAll('.chat-view .c-card')].map(c=>{
  const b=c.querySelector('.c-card-body'), d=c.closest('.c-dialog'), cs=getComputedStyle(d);
  const col=d.clientWidth-parseFloat(cs.paddingLeft)-parseFloat(cs.paddingRight);
  const f=c.querySelector('iframe');
  return {agent:c.dataset.agent, state:c.dataset.state, w:c.getBoundingClientRect().width, col:col,
          bh:b?b.getBoundingClientRect().height:null, idx:[...d.children].indexOf(c),
          marker: f? (f.srcdoc||'').includes('card-start') : null, deleted: (c.querySelector('.c-card-deleted')||{}).textContent||null};
}))()"""


class Ctx:
    """The run: host, page, mock, and the small helpers every run uses."""

    def __init__(self, sc: dict, tmp: Path):
        self.sc, self.tmp = sc, tmp
        self.cwd = tmp / "work"
        self.cwd.mkdir(parents=True)
        self.host: Host | None = None
        self.page: Page | None = None
        self.chrome: Chrome | None = None
        self.mock = None
        self.url = ""
        self.pg = ""  # the page's guid
        self.cell = ""  # the chat cell's id
        self.places: list[tuple] = []  # the cards' places in the last redraw from history

    @property
    def port(self) -> int:
        assert self.host is not None
        return self.host.port

    async def call(self, target: str, payload: dict) -> dict:
        return await host_call(self.port, target, payload)

    async def js(self, expr: str):
        assert self.page is not None
        return await self.page.js(expr)

    async def until(self, expr: str, timeout: float = 20.0) -> bool:
        assert self.page is not None
        return await self.page.until(expr, timeout)

    async def cards(self) -> list[dict]:
        return await self.js(CARDS_JS) or []

    async def card(self, agent: str) -> dict | None:
        return next((c for c in await self.cards() if c["agent"] == agent), None)

    async def wait_card(self, agent: str, state: str = "live", timeout: float = 30.0) -> dict | None:
        end = time.time() + timeout
        while time.time() < end:
            c = await self.card(agent)
            if c is not None and c["state"] == state:
                return c
            await asyncio.sleep(0.25)
        return await self.card(agent)

    async def say(self, text: str) -> None:
        """Type into the chat's input and press Enter."""
        assert self.page is not None
        await self.page.js("document.querySelector('.chat-view .c-input').focus()")
        await self.page.cdp("Input.insertText", text=text)
        for typ in ("keyDown", "keyUp"):
            await self.page.cdp("Input.dispatchKeyEvent", type=typ, key="Enter", code="Enter", windowsVirtualKeyCode=13,
                                **({"text": "\r"} if typ == "keyDown" else {}))

    async def click(self, sel: str, shift: bool = False) -> bool:
        assert self.page is not None
        if not await self.page.until(f"(()=>{{const e=document.querySelector({json.dumps(sel)}); if(!e) return false;"
                                     "const r=e.getBoundingClientRect(); return r.width>0&&r.height>0;})()", 10):
            return False
        await self.page.js(f"document.querySelector({json.dumps(sel)}).scrollIntoView({{block:'center'}})")
        await asyncio.sleep(0.2)
        r = await self.page.rect(sel)
        assert r is not None
        mods = 8 if shift else 0
        await self.page.mouse("mousePressed", r["x"] + r["w"] / 2, r["y"] + r["h"] / 2, modifiers=mods)
        await self.page.mouse("mouseReleased", r["x"] + r["w"] / 2, r["y"] + r["h"] / 2, modifiers=mods)
        return True

    async def open_page(self) -> None:
        assert self.page is not None
        await self.page.open(self.url)
        await self.page.until("!!document.getElementById('canvas') && !!document.querySelector('.canvas-world')", 30)
        await self.page.until("!!document.querySelector('.chat-view[data-ready]')", 30)

    def requests_with(self, needle: str) -> list[dict]:
        return [t for t in self.mock.turns() if needle in json.dumps(t["body"])] if self.mock else []


def grants_ok(r: dict) -> bool:
    return not r.get("error")


async def emit_to(port: int, target: str, payload: dict) -> None:
    """An EMIT to a host agent's inbox, the way a tab makes one (a ws `emit` frame through `user`'s leg):
    what the agent's watchers receive. The agent's own handler is not asked."""
    import websockets

    async with websockets.connect(f"ws://127.0.0.1:{port}/user/ws", origin=f"http://127.0.0.1:{port}") as ws:
        await ws.send(json.dumps({"type": "emit", "target": target, "payload": payload}))
        await asyncio.sleep(0.5)


async def boot(ctx: Ctx, bad: list[str]) -> bool:
    """Host, Chrome, the chat's AI, the chat cell. False (with `bad` said) when it cannot be made."""
    from mocks.vllm import VllmMock

    sc = ctx.sc
    w = sc["window"]
    binary = build_bin(ctx.tmp)
    ctx.host = Host(binary, ctx.cwd, free_port())
    cdp_port = free_port()
    exe = os.environ.get("CHROME") or MAC_CHROME
    if not Path(exe).exists():
        raise SystemExit(f"no Chrome at {exe}: set CHROME")
    ctx.chrome = Chrome([exe, "--headless=new", "--use-angle=swiftshader", "--enable-unsafe-swiftshader", DESKTOP_POINTER,
                         f"--window-size={w['width']},{w['height']}", f"--remote-debugging-port={cdp_port}",
                         f"--user-data-dir={ctx.tmp / 'chrome'}", "--no-first-run", "about:blank"], ctx.tmp / "chrome.log", cdp_port)
    ctx.host.start()
    assert ctx.host.url is not None
    ctx.url = ctx.host.url
    ws = await attach(ctx.chrome)
    ctx.page = Page(ws)
    await ctx.page.cdp("Runtime.enable")
    await ctx.page.cdp("Page.enable")
    await ctx.page.viewport(w["width"], w["height"])
    await ctx.page.open(ctx.url)
    if not await ctx.page.until("!!document.getElementById('canvas') && !!document.querySelector('.canvas-world')", 30):
        bad.append("the canvas never drew")
        return False

    # ── the chat's AI: the mock (scripted) or the plug BACKEND names (model)
    if sc.get("mode") == "model":
        from backends import backend

        be = backend()
        record = be.record()
        if be.key_ref:
            from helpers import creds

            refs = creds.env_refs()
            made = await ctx.call("kernel", creds.store_record("kernel_state", refs) | {"type": "create_agent"})
            if made.get("error"):
                bad.append(f"the credential store: {made}")
                return False
    else:
        ctx.mock = VllmMock(model="mock-chat").start()
        ctx.mock.responder = responder
        record = {"handler_module": "vllm_backend.tools", "endpoint": ctx.mock.url, "model": "mock-chat", "max_tokens": 8192,
                  "context_window": 262144, "kv_slots": 4, "kv_overflow": "evict"}
    # wired both ways: the AI reaches the memory store and the kernel's create_agent; the page's user reaches the AI
    for target in ("memory", "kernel_state"):
        cur = next((a for a in (await ctx.call("kernel", {"type": "list_agents"})).get("agents", []) if a.get("id") == target), {})
        r = await ctx.call("kernel", {"type": "update_agent", "id": target, "callable_by": {**(cur.get("callable_by") or {}), AI: "*"}})
        if r.get("error"):
            bad.append(f"{target} would not admit the AI: {r}")
            return False
    can_call = {"$self": "*", "memory": "*", "kernel_state": "*", "canvas_root": "*"}
    if record.get("credentials"):
        can_call["creds"] = ["get"]
    made = await ctx.call("kernel", {"type": "create_agent", "parent_id": "canvas_root", "id": AI, **record,
                                     "memory_store_id": "memory", "can_call": can_call,
                                     "callable_by": {"$self": "*", "$parent": "*", "user": "*"}})
    if made.get("error") or made.get("id") != AI:
        bad.append(f"the AI was not made: {made}")
        return False
    if record.get("credentials"):
        from helpers import creds

        cur = next((a for a in (await ctx.call("kernel", {"type": "list_agents"})).get("agents", []) if a.get("id") == creds.CREDS), {})
        await ctx.call("kernel", {"type": "update_agent", "id": creds.CREDS, "callable_by": {**(cur.get("callable_by") or {}), AI: ["get"]}})

    # ── the chat cell, placed on the page's view from the host
    refl = await ctx.call("kernel", {"type": "reflect"})
    peers = refl.get("peers") or []
    if not peers:
        bad.append(f"the page is no peer of the host: {str(refl)[:200]}")
        return False
    ctx.pg = peers[0]["guid"]
    placed = await ctx.call(f"{ctx.pg}|view_root", {"type": "place", "backend_id": AI, "x": 40, "y": 30, "width": 620, "height": 800})
    if not placed.get("placed"):
        bad.append(f"the chat cell was not placed: {placed}")
        return False
    ctx.cell = placed["placed"]
    if not await ctx.until("!!document.querySelector('.chat-view[data-ready]')", 30):
        bad.append("the chat cell never became ready")
        return False
    return True


# ── the runs ───────────────────────────────────────────────────────────────────────
async def run_embed(ctx: Ctx, bad: list[str]) -> None:
    assert ctx.mock is not None
    await ctx.say("show me a card")
    c = await ctx.wait_card("card_a")
    if c is None or c["state"] != "live":
        bad.append(f"no live card after the chain: {c}; page cards: {await ctx.cards()}; requests: {len(ctx.mock.turns())}")
        return
    # the panel measures itself after it loads: the card takes its height then
    end = time.time() + 15
    while time.time() < end and (c["bh"] or 0) < 290:
        await asyncio.sleep(0.25)
        c = await ctx.card("card_a") or c
    if abs(c["w"] - c["col"]) > 2:
        bad.append(f"the card is {c['w']} px wide, the chat column {c['col']} px")
    if not (60 <= c["bh"] <= CAP):
        bad.append(f"the card's height {c['bh']} is not defined within [60, {CAP}]")
    if c["bh"] < 290:
        bad.append(f"the card did not follow its panel's 300 px height: {c['bh']}")
    if c["marker"] is not True:
        bad.append("the panel's own body is not in the card's frame")
    # what the model read back from `embed`: the box and the cap
    sent = ctx.requests_with("max_height")
    if not sent or not re.search(r"<max_height>\s*1200\s*</max_height>", json.dumps(sent[-1]["body"]).replace("\\n", "")):
        bad.append("the embed reply the model read does not carry the 1200 px cap")
    if not ctx.requests_with("<width>100%</width>"):
        bad.append("the embed reply the model read does not carry the 100% width")
    # the row is in the AI's history, as a card row
    h = await ctx.call(AI, {"type": "history"})
    if not any(r.get("kind") == "embed" and r.get("head") == "card_a" for r in h.get("rows", [])):
        bad.append(f"no embed row in the history: {h}")


async def run_live_update(ctx: Ctx, bad: list[str]) -> None:
    before = await ctx.card("card_a")
    r = await emit_to(ctx.port, "card_a", {"type": "note", "text": "updated", "h": 460})
    end = time.time() + 15
    got = before
    while time.time() < end:
        got = await ctx.card("card_a")
        if got is not None and got["bh"] is not None and got["bh"] >= 450:
            break
        await asyncio.sleep(0.25)
    if got is None or got["bh"] is None or got["bh"] < 450:
        bad.append(f"the card did not redraw on the agent's update (the call said {r}): {before} -> {got}")


async def run_overflow(ctx: Ctx, bad: list[str]) -> None:
    assert ctx.mock is not None
    await ctx.say("now a big one")
    c = await ctx.wait_card("card_b")
    if c is None or c["state"] != "live":
        bad.append(f"no live card_b: {c}")
        return
    end = time.time() + 15
    while time.time() < end and (c["bh"] or 0) < CAP - 1:
        await asyncio.sleep(0.25)
        c = await ctx.card("card_b") or c
    if c["bh"] > CAP + 1:
        bad.append(f"the card is {c['bh']} px tall, over the {CAP} px cap")
    if c["bh"] < CAP - 1:
        bad.append(f"a 3000 px panel's card should be capped AT {CAP}: {c['bh']}")
    end = time.time() + 30
    while time.time() < end and not ctx.requests_with("[chat card card_b]"):
        await asyncio.sleep(0.5)
    told = ctx.requests_with("[chat card card_b]")
    if not told:
        bad.append("the AI was not told its card overflows")
    elif "Redo the panel" not in json.dumps(told[-1]["body"]):
        bad.append("the notice does not say to redo the panel")


async def snapshot(ctx: Ctx) -> list[tuple]:
    return [(c["agent"], c["idx"]) for c in await ctx.cards()]


async def run_reload(ctx: Ctx, bad: list[str]) -> None:
    # the live chat holds what a redraw from history does not (the tool blocks), so the
    # PLACES are compared between two redraws, and the order of the cards with the live one
    live = [a for a, _ in await snapshot(ctx)]
    redraws = []
    for _ in range(2):
        await ctx.open_page()
        for a in live:
            await ctx.wait_card(a)
        redraws.append(await snapshot(ctx))
    ctx.places = redraws[-1]
    if [a for a, _ in redraws[0]] != live:
        bad.append(f"the cards after a reload {redraws[0]} are not those before {live}")
    if redraws[0] != redraws[1]:
        bad.append(f"two redraws put the cards in different places: {redraws[0]} / {redraws[1]}")
    for c in await ctx.cards():
        if c["state"] != "live" or abs(c["w"] - c["col"]) > 2:
            bad.append(f"a card after the reload is not live at full width: {c}")


async def run_restart(ctx: Ctx, bad: list[str]) -> None:
    assert ctx.host is not None
    before = ctx.places
    code = ctx.host.stop()
    if code != 0:
        bad.append(f"host exited {code} on SIGTERM")
    ctx.host.start()
    await ctx.open_page()
    for a, _ in before:
        await ctx.wait_card(a, timeout=40)
    after = await snapshot(ctx)
    if after != before:
        bad.append(f"the cards after a restart {after} are not those before {before}")
    for c in await ctx.cards():
        if c["state"] != "live":
            bad.append(f"a card after the restart is not live: {c}")


async def run_delete_open(ctx: Ctx, bad: list[str]) -> None:
    r = await ctx.call("kernel", {"type": "delete_agent", "id": "card_b"})
    if r.get("error"):
        bad.append(f"delete_agent card_b: {r}")
        return
    c = await ctx.wait_card("card_b", "deleted", 20)
    if c is None or c["state"] != "deleted":
        bad.append(f"the card did not turn deleted at once: {c}")
    a = await ctx.card("card_a")
    if a is None or a["state"] != "live":
        bad.append(f"the other card was touched: {a}")


async def run_delete_closed(ctx: Ctx, bad: list[str]) -> None:
    assert ctx.page is not None
    # the chat CLOSED: the page leaves; the agent is deleted meanwhile
    await ctx.page.cdp("Page.navigate", url="about:blank")
    r = await ctx.call("kernel", {"type": "delete_agent", "id": "card_a"})
    if r.get("error"):
        bad.append(f"delete_agent card_a: {r}")
        return
    await ctx.open_page()
    a = await ctx.wait_card("card_a", "deleted", 30)
    if a is None or a["state"] != "deleted" or a["deleted"] != "[deleted from canvas]":
        bad.append(f"after a reload the deleted agent's card is not the placeholder: {a}")
        return
    if (await ctx.wait_card("card_b", "deleted", 30)) is None or (await ctx.card("card_b"))["state"] != "deleted":
        bad.append(f"card_b (deleted earlier) should be a placeholder too: {await ctx.card('card_b')}")
    asked: list[str] = []
    await ctx.js("window.__asked=[]; window.__yes=false; window.confirm=(m)=>{window.__asked.push(m); return window.__yes;}")
    # BACK under the same id (card_b was deleted too: make it again): the press re-attaches, no question
    made = await ctx.call("kernel", view_record("card_b"))
    if made.get("error"):
        bad.append(f"card_b was not made again: {made}")
    elif await ctx.click(".c-card[data-agent=card_b] .c-card-deleted"):
        b = await ctx.wait_card("card_b", "live", 15)
        if b is None or b["state"] != "live":
            bad.append(f"the press did not re-attach a card whose agent is back: {await ctx.card('card_b')}")
    else:
        bad.append("card_b has no placeholder to press")
    # GONE: the press asks; "no" keeps the card, "yes" unembeds it
    await ctx.click(".c-card[data-agent=card_a] .c-card-deleted")
    await asyncio.sleep(1.0)
    asked = await ctx.js("window.__asked") or []
    a = await ctx.card("card_a")
    if len(asked) != 1 or a is None:
        bad.append(f"pressing the placeholder of a gone agent should ask once and keep the card on no: asked {asked}, card {a}")
    await ctx.js("window.__yes=true")
    await ctx.click(".c-card[data-agent=card_a] .c-card-deleted")
    end = time.time() + 15
    while time.time() < end and await ctx.card("card_a") is not None:
        await asyncio.sleep(0.25)
    if await ctx.card("card_a") is not None:
        bad.append("answering yes did not remove the card")
    h = await ctx.call(AI, {"type": "history"})
    if not any(r.get("kind") == "unembed" and r.get("head") == "card_a" for r in h.get("rows", [])):
        bad.append(f"no unembed row after the answer: {h}")


async def run_unembed(ctx: Ctx, bad: list[str]) -> None:
    assert ctx.mock is not None
    await ctx.say("another card")
    c = await ctx.wait_card("card_c", timeout=40)
    if c is None:
        bad.append(f"the model's embed drew no card: page cards {await ctx.cards()}, requests {len(ctx.mock.turns())}, "
                   f"history tail {[(r.get('kind'), r.get('head')[:40]) for r in (await ctx.call(AI, {'type': 'history'})).get('rows', [])[-6:]]}")
        return
    # drawn when the embed answered, not when the turn ended
    await ctx.click(".c-card[data-agent=card_c] .c-card-more")
    if not await ctx.click(".c-card[data-agent=card_c] .c-card-remove"):
        bad.append("no 'remove card' in the card's menu")
        return
    end = time.time() + 15
    while time.time() < end and await ctx.card("card_c") is not None:
        await asyncio.sleep(0.25)
    if await ctx.card("card_c") is not None:
        bad.append("the card is still there after 'remove card'")
    still = await ctx.call("card_c", {"type": "reflect"})
    if still.get("error"):
        bad.append(f"unembed touched the agent: {still}")
    await ctx.open_page()
    await asyncio.sleep(2.0)
    if await ctx.card("card_c") is not None:
        bad.append("the removed card came back after a reload")


async def run_canvas_and_chat(ctx: Ctx, bad: list[str]) -> None:
    assert ctx.mock is not None
    await ctx.say("one more card")
    if (await ctx.wait_card("card_d", timeout=40)) is None:
        bad.append("no card for card_d in the chat")
        return
    placed = await ctx.call(f"{ctx.pg}|view_root", {"type": "place", "backend_id": "card_d", "x": 700, "y": 60})
    if not placed.get("placed"):
        bad.append(f"card_d was not placed on the canvas: {placed}")
        return
    cell = placed["placed"]
    if not await ctx.until(f"!!document.querySelector('.agent-frame[data-id={cell}]')", 15):
        bad.append("the placed cell drew no frame")
    h = await ctx.call(f"{ctx.pg}|view_root", {"type": "hide", "id": cell})
    if h.get("error"):
        bad.append(f"hide {cell}: {h}")
    if not await ctx.until(f"!document.querySelector('.agent-frame[data-id={cell}]')", 15):
        bad.append("the hidden cell is still drawn")
    await asyncio.sleep(1.0)
    c = await ctx.card("card_d")
    if c is None or c["state"] != "live":
        bad.append(f"hiding the canvas cell changed the chat card: {c}")


async def run_model_card(ctx: Ctx, bad: list[str]) -> None:
    await ctx.say(ctx.sc["task"])
    deadline = time.time() + float((ctx.sc.get("limits") or {}).get("idle_seconds", 240))
    cards: list[dict] = []
    while time.time() < deadline:
        cards = [c for c in await ctx.cards() if c["state"] == "live" and c["bh"] is not None and c["bh"] > 60]
        if cards:
            break
        await asyncio.sleep(2.0)
    if not cards:
        bad.append(f"no card appeared in {ctx.sc['limits']['idle_seconds']} s")
        return
    await asyncio.sleep(3.0)  # the panel measures itself, the card follows
    c = (await ctx.cards())[0]
    if abs(c["w"] - c["col"]) > 2:
        bad.append(f"the card is {c['w']} px wide, the chat column {c['col']} px")
    if not (60 <= c["bh"] <= CAP):
        bad.append(f"the card's height {c['bh']} is outside [60, {CAP}]")
    over = await ctx.js("(()=>{const f=document.querySelector('.chat-view .c-card iframe'); return f? f.getBoundingClientRect().width : -1;})()")
    if over is not None and over > c["col"] + 2:
        bad.append("the card's frame is wider than the column")


RUNS = {
    "embed": run_embed, "live-update": run_live_update, "overflow-hint": run_overflow, "reload": run_reload,
    "restart": run_restart, "delete-open": run_delete_open, "delete-closed": run_delete_closed,
    "unembed": run_unembed, "canvas-and-chat": run_canvas_and_chat, "model-card": run_model_card,
}


async def run_all(sc: dict, tmp: Path) -> bool:
    ctx = Ctx(sc, tmp)
    results: list[tuple[str, list[str]]] = []
    try:
        bad: list[str] = []
        if not await boot(ctx, bad):
            results.append(("boot", bad))
        else:
            for name in sc["runs"]:
                bad = []
                try:
                    await RUNS[name](ctx, bad)
                except Exception as e:  # a run that cannot go on says why, and the next one still runs
                    bad.append(f"{type(e).__name__}: {e}")
                results.append((name, bad))
            assert ctx.page is not None
            exc = [line for line in ctx.page.logs if "EXC" in line or os.environ.get("E2E_LOGS")]
            if exc:
                results.append(("page exceptions", exc[:5]))
    finally:
        if ctx.host is not None and ctx.host.proc is not None:
            ctx.host.stop()
        if ctx.mock is not None:
            ctx.mock.stop()
        if ctx.chrome is not None:
            ctx.chrome.proc.terminate()
            try:
                ctx.chrome.proc.wait(10)
            except subprocess.TimeoutExpired:
                ctx.chrome.proc.kill()
    for name, bad in results:
        print(f"  {'✓' if not bad else '✗'} {name}", flush=True)
        for x in bad:
            print(f"      {x}", flush=True)
    return all(not bad for _, bad in results)


def main(argv: list[str]) -> int:
    import yaml

    if not argv or argv[0].startswith("-"):
        raise SystemExit("usage: canvas_chat.py <scenario-id>")
    sc = yaml.safe_load(_scenario_path(argv[0]).read_text())
    print(f"{argv[0]} · {sc['title']}", flush=True)
    if sc.get("mode") == "model":
        from backends import backend

        try:
            be = backend()
        except SystemExit as e:
            print(f"E2E-SKIP: no model plug — {e}", flush=True)
            return 0
        if blockers := be.blockers():
            print(f"E2E-SKIP: {be.name} is not ready — {blockers[0]}", flush=True)
            return 0
    tmp = Path(tempfile.mkdtemp(prefix="e2e-chat-cards-"))
    try:
        ok = asyncio.run(run_all(sc, tmp))
    finally:
        if run_dir := os.environ.get("E2E_RUN_DIR"):
            for log in list(tmp.glob("chrome*.log")) + [tmp / "host.log"]:
                if log.exists():
                    shutil.copy(log, Path(run_dir) / f"canvas_chat.{log.name}")
        if not os.environ.get("E2E_KEEP"):
            shutil.rmtree(tmp, ignore_errors=True)
        else:
            print(f"  kept {tmp}")
    verdict(ok, "all runs green" if ok else "failures above")
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
