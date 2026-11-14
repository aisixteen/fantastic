"""E157 chat_view_renders — the html chat view (`fantastic-ai-core/src/chat_view.html`,
the `view` the chat's own `reflect` returns) drawn by a canvas cell, from a scripted model:

a) the person says hello: a person bubble; the model answers in words with a `code`
   span and a run of two calls: an agent bubble with a code chip, and ONE row
   `2 tool calls` that opens to its calls;
b) the person asks a question: the model sends the person an inquiry (`user inquiry`,
   kind single): a card with the options, the header's state `asks you`;
c) a click on an option answers it: `answered`, the state pill gone;
d) a page reload: the cell draws the same history again.

The model is the parity chat stub: each request's newest message picks the turn."""

from __future__ import annotations

import asyncio
import json
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[2]))
from _lib.canvas_live import Rig, verdict, say, chat_eval, chat_rows, chat_drawn  # noqa: E402

E2E = Path(__file__).resolve().parents[6] / "e2e"
DONE = '<call name="done"></call>'
LIST = "<call name=\"send\"><routing><target_id>user</target_id></routing><payload><type>inquiries</type><asker>nobody</asker></payload></call>"
ASK = ("<call name=\"send\"><routing><target_id>user</target_id></routing><payload><type>inquiry</type>"
       "<text>Which colour?</text><kind>single</kind><options><item>red</item><item>blue</item></options></payload></call>")


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


def main() -> bool:
    import socket
    sys.path.insert(0, str(E2E / "parity"))
    from stubs.openai_chat import Handler, OpenAIChatStub  # noqa: E402

    rig = Rig(__file__, live=False)
    rig.build("vllm")
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        port = s.getsockname()[1]
    stub = OpenAIChatStub(_Rec(rig.art / "stub"), "stub", port)

    def responder(req: dict, _raw: bytes):
        msgs = req.get("messages") or []
        last = msgs[-1].get("content") if msgs else ""
        last = last if isinstance(last, str) else json.dumps(last)
        if last.startswith("[reply to") and "inquiry" in last.split("\n", 1)[0]:
            return "Thanks, noted. " + DONE
        if last.startswith("[reply to"):
            return "Both calls are back. " + DONE
        if "hello" in last:
            return "Listing with `ls` now.\n" + LIST + "\n" + LIST
        if "ask me" in last:
            return ASK
        return "ok " + DONE
    stub.responder = responder
    stub.start(Handler)
    rig.make_home({"handler_module": "vllm_backend.tools", "endpoint": stub.base_url(), "model": "stub",
                   "context_window": 32768, "max_tokens": 4096, "kv_slots": 4, "kv_overflow": "evict"})
    rig.init()
    r: dict = {}
    info: dict = {}

    async def until(pg, pred, secs: float = 25) -> dict:
        got: dict = {}
        for _ in range(int(secs * 2)):
            got = await chat_rows(pg)
            if got and pred(got):
                return got
            await asyncio.sleep(0.5)
        return got

    async def open_chat(pg) -> None:
        """The chat is made as the canvas makes every AI (`canvas_root create`: a provider
        and its chat, the chat holding `user` `inquiry`) and shown by a cell the root view
        places; the cell draws the view the chat's `reflect` returns."""
        from canvas import host_call

        made = await host_call(rig.port, "canvas_root", {"type": "create", "record": {
            "id": "chat_e157", "handler_module": "vllm_backend.tools", "endpoint": stub.base_url(), "model": "stub",
            "context_window": 32768, "max_tokens": 4096, "kv_slots": 4, "kv_overflow": "evict",
            "can_call": {"user": ["inquiry", "inquiries"]}}})
        info["made"] = made
        placed = await host_call(rig.port, f"{rig.page_guid()}|view_root",
                                 {"type": "place", "backend_id": made.get("id", "chat_e157"), "x": 300, "y": 200, "width": 440, "height": 520})
        info["placed"] = placed
        if not await chat_drawn(pg, 30):
            raise SystemExit(f"the chat view never drew: {made} {placed}")

    async def drive() -> None:
        async with rig.page(1600, 1000) as pg:
            await asyncio.sleep(1.5)
            await open_chat(pg)
            got = await chat_rows(pg)
            info["head"] = {k: got.get(k) for k in ("name", "model")}
            r["1 the view draws, header names the chat and its model"] = bool(got.get("name")) and got.get("model") == "stub"
            await say(pg, "hello there")
            got = await until(pg, lambda g: any("Both calls are back" in m for m in g["agent"]) and g["tools"])
            info["turn1"] = got
            r["2 a person bubble"] = any("hello there" in m for m in got.get("user", []))
            r["3 an agent bubble with a code chip"] = any("Listing with" in m for m in got.get("agent", [])) and "ls" in got.get("code", [])
            tools = got.get("tools", [])
            r["4 ONE row, 2 tool calls, two entries"] = len(tools) == 1 and tools[0]["n"] == "2 tool calls" and tools[0]["calls"] == 2
            await say(pg, "please ask me")
            got = await until(pg, lambda g: g["inquiries"])
            info["turn2"] = got
            inq = (got.get("inquiries") or [{}])[0]
            r["5 an inquiry card with its options, state asks you"] = (inq.get("q") == "Which colour?" and inq.get("options") == ["red", "blue"]
                                                                   and got["state"]["pill"].startswith("asks you") and not got["state"]["pill_hidden"])
            await chat_eval(pg, "(()=>{[...d.querySelectorAll('#dialog .inq .row > button')].find(b=>b.textContent==='red').click(); return true;})()")
            got = await until(pg, lambda g: g["inquiries"] and g["inquiries"][0]["answered"] and g["state"]["pill_hidden"])
            info["turn3"] = got
            inq = (got.get("inquiries") or [{}])[0]
            r["6 a click answers it: answered, asks you gone"] = bool(inq.get("answered")) and "red" in inq.get("status", "") and got["state"]["pill_hidden"]
            before = (len(got["user"]), len(got["agent"]), len(got["tools"]), len(got["inquiries"]))
            await pg.open(rig.url)
            ok = False
            for _ in range(60):
                if await chat_drawn(pg, 1):
                    ok = True
                    break
                await asyncio.sleep(0.5)
            got = await until(pg, lambda g: len(g["user"]) >= before[0] and len(g["tools"]) >= before[2]) if ok else {}
            info["reloaded"] = got
            r["7 a reload replays the history"] = bool(got) and any("hello there" in m for m in got["user"]) and len(got["tools"]) == 1 and got["tools"][0]["n"] == "2 tool calls" \
                and any("Both calls are back" in m for m in got["agent"])
            info["page_errors"] = [x for x in pg.logs if "WebGL" not in x][:8]
            r["8 no page error"] = not info["page_errors"]

    try:
        asyncio.run(drive())
    finally:
        rig.stop()
        stub.stop()
    rig.summary["checks"] = r
    rig.summary["info"] = info
    print(json.dumps(info, indent=1, default=str)[:4000])
    failed = [k for k, v in r.items() if not v]
    return verdict(*rig.finish(not failed, f"{len(r) - len(failed)}/{len(r)} pass" + ("" if not failed else f" — failed: {'; '.join(failed)}")))


if __name__ == "__main__":
    sys.exit(0 if main() else 1)
