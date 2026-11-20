"""The line summary and the fold (#709), for the free Rust probes E163 and E164.

One `ai_chat.tools` chat on the PACKAGED host, its record offering `line_summaries` and folding
(`context_strategy: fold`) over a small window, completes through a `vllm_backend` provider whose
endpoint is a scripted vLLM mock (`mocks/vllm.py`). The chat's events are watched over the ws
face. (The sidebar agent that passes the line on lives in the `fantastic` product, not in the
packaged kernel host these probes boot: its units cover it.) NO MODEL: the mock answers
each turn from the probe's script, and the chat's own asks by their text — a fold's summariser
ask with a summary, a `context_recall` with an empty choice.

`Rig.turn(text, reply)` is one person's message and the model's scripted answer;
`Rig.fold_ids()` the folds in the chat's view now; `Rig.fold(id)` one fold with its members,
read back through `history {fold}` and `recall`.
"""

from __future__ import annotations

import asyncio
import json
import re
import threading
import time
from pathlib import Path

from helpers.host.driver import HostKernel
from helpers.host.ws import WSWatcher
from mocks.vllm import VllmMock

AI, PROV = "ai", "prov_ai"
MODEL = "mock-scripted"
DONE = '<call name="done"></call>'
#: The summariser's asks: titled (no line in the stretch) or the body alone (a line titles it).
FOLD_ASK = "FOLD this finished piece of work"
BODY_ASK = "Its title is written already"
TITLED_ASK = "First line: a one-sentence title"
RECALL_ASK = "You are rebuilding your own working context"
#: A small window: a fold comes after about ten exchanges of FILLER.
WINDOW, RESERVE = 6000, 500
FILLER = 1600
LINE_RE = re.compile(r'<call name="line_summary"><text>([^<]*)</text></call>')


def line(text: str) -> str:
    return f'<call name="line_summary"><text>{text}</text></call>'


def messages(req: dict) -> list:
    """The chat messages of a request: a recorded turn (`{body: …}`, `mock.turns()`) or the
    body itself (what the mock's `responder` and `turn` get)."""
    body = req if "messages" in req else (req.get("body") or {})
    return body.get("messages") or []


def content_of(m: dict) -> str:
    c = m.get("content")
    return c if isinstance(c, str) else "".join(
        b.get("text", "") for b in c or [] if isinstance(b, dict))


def own_ask(req: dict) -> str:
    """The text of the chat's own ask (a fold's summariser, a `context_recall`) when `req`
    is one, else "": the ask's words are in its messages, never in a turn of the chat."""
    text = "\n".join(content_of(m) for m in messages(req))
    return text if FOLD_ASK in text or RECALL_ASK in text else ""


class SizedVllmMock(VllmMock):
    """The vLLM mock reporting a prompt size near the chat's own estimate (a quarter of the
    request's characters), not the base mock's fixed 100: the chat calibrates its budget by
    that report, and a tiny one would stretch the 6000 token window past any fold."""

    def turn(self, h, req, text):
        self.prompt_tokens = max(1, len(blob(req)) // 4)
        super().turn(h, req, text)


class Watch:
    """One agent's events over the ws face, collected in a daemon thread."""

    def __init__(self, port: int, aid: str):
        self.seen: list[dict] = []
        self.error: BaseException | None = None
        self.ready = threading.Event()
        self.cond = threading.Condition()
        threading.Thread(target=lambda: asyncio.run(self._run(port, aid)), daemon=True).start()

    async def _run(self, port: int, aid: str) -> None:
        w = WSWatcher(port, aid)
        try:
            await w.connect()
        except Exception as e:  # noqa: BLE001 — kept, raised by the reader
            self.error = e
            return
        finally:
            self.ready.set()
        async for ev in w.events():
            with self.cond:
                self.seen.append(ev)
                self.cond.notify_all()

    def of(self, kind: str) -> list[dict]:
        with self.cond:
            if self.error is not None:
                raise self.error
            return [e for e in self.seen if e.get("type") == kind]

    def wait(self, pred, timeout: float = 20.0) -> bool:
        with self.cond:
            return self.cond.wait_for(lambda: any(pred(e) for e in self.seen), timeout)


class Rig:
    def __init__(self, spec: dict, work: Path, summary_body: str, summary_title: str):
        self.hk = HostKernel(spec, work, reach=[AI, PROV, "memory", "creds"]).boot()
        self.mock = SizedVllmMock(model=MODEL).start()
        self.body, self.title = summary_body, summary_title
        self.mock.responder = self._own_asks
        self.bad: list[str] = []
        self.chat_ev: Watch | None = None

    # ── the mock's answers to the chat's own asks ─────────────────────────────
    def _own_asks(self, req: dict) -> str | None:
        ask = own_ask(req)
        if FOLD_ASK in ask:
            # the body alone when the stretch has its line; a title line first otherwise
            return self.body if BODY_ASK in ask else f"{self.title}\n\n{self.body}"
        if RECALL_ASK in ask:
            return "{}"
        return None

    def asks(self, marker: str) -> list[str]:
        return [a for a in map(own_ask, self.mock.turns()) if FOLD_ASK in a and marker in a]

    # ── checks ────────────────────────────────────────────────────────────────
    def need(self, cond: bool, what: str) -> None:
        print(f"  {'ok ' if cond else 'BAD'} {what}", flush=True)
        if not cond:
            self.bad.append(what)

    # ── wiring ────────────────────────────────────────────────────────────────
    def wire(self) -> None:
        hk = self.hk
        r = hk.send("kernel", {"type": "list_agents"})
        mem = next(a for a in r.get("agents") or [] if a.get("id") == "memory")
        r = hk.send(hk.root_id, {"type": "update_agent", "id": "memory",
                                 "callable_by": {**dict(mem.get("callable_by") or {}),
                                                 AI: "*", PROV: "*"}})
        self.need(r.get("updated") is True, f"memory accepts the chat: {str(r)[:200]}")
        by = {"$self": "*", "$parent": "*", "rest": "*", "ws": "*", "web": "*"}
        rec = hk.create(parent_id="rest", handler_module="vllm_backend.tools", id=PROV,
                        endpoint=self.mock.url, model=MODEL, max_tokens=8192,
                        context_window=262144, kv_slots=4, kv_overflow="evict",
                        memory_store_id="memory", can_call={"$self": "*", "memory": "*"},
                        callable_by={"$self": "*", "$parent": "*", AI: "*"})
        self.need(rec.get("id") == PROV, f"create {PROV}: {str(rec)[:200]}")
        rec = hk.create(parent_id="rest", handler_module="ai_chat.tools", id=AI,
                        provider_id=PROV, model=MODEL, context_window=WINDOW,
                        output_reserve=RESERVE, context_strategy="fold",
                        line_summaries=True, system_prompt="You are a test chat.",
                        memory_store_id="memory",
                        can_call={"$self": "*", "memory": "*", PROV: ["complete", "watch"]},
                        callable_by=by)
        self.need(rec.get("id") == AI, f"create {AI}: {str(rec)[:200]}")
        self.chat_ev = Watch(hk.port, AI)
        self.chat_ev.ready.wait(10)
        time.sleep(0.5)  # the ws face acks a watch with nothing: let it register

    # ── driving ───────────────────────────────────────────────────────────────
    def idle(self, bound: float = 60) -> bool:
        end = time.monotonic() + bound
        while time.monotonic() < end:
            tasks = self.hk.send("kernel", {"type": "ps"}).get("tasks") or []
            if not any(t.get("agent") == AI for t in tasks):
                return True
            time.sleep(0.05)
        return False

    def answered(self) -> int:
        """How many of the mock's requests were the chat's turns (not its own asks)."""
        return sum(1 for t in self.mock.turns() if own_ask(t) == "")

    def turn(self, text: str, reply: str, bound: float = 60) -> None:
        before = self.answered()
        self.mock.reply(reply)
        out = self.hk.send(AI, {"type": "send", "text": text})
        if out.get("error"):
            self.need(False, f"the send is accepted: {str(out)[:200]}")
        end = time.monotonic() + bound
        while self.answered() <= before and time.monotonic() < end:
            time.sleep(0.05)
        if self.answered() <= before:
            self.need(False, f"the scripted answer is asked for: {self.answered()} turns")
        if not self.idle():
            self.need(False, "the chain settles")

    def pages(self, ask: dict) -> tuple[list[dict], dict]:
        """Every row of a `history` ask, page by page (`next` → `after`), and the first page.

        READ UNDER A WIDE WINDOW: a page answers under the chat's inline cap (an eighth of
        its budget), and in the 6000 token window one row's digest — an answer's
        `attributes.parts` — is over it, which ends the paging there. The window is widened
        for the read and set back before the next turn."""
        self._window(262144)
        try:
            first = h = self.hk.send(AI, {"type": "history", "limit": 500, **ask})
            rows = list(h.get("rows") or [])
            while h.get("next") is not None:
                h = self.hk.send(AI, {"type": "history", "limit": 500, **ask, "after": h["next"]})
                got = h.get("rows") or []
                if not got:
                    break
                rows += got
        finally:
            self._window(WINDOW)
        if first.get("total") is not None and len(rows) != first["total"]:
            self.need(False, f"history {ask}: {len(rows)} rows paged of {first['total']}")
        return rows, first

    def _window(self, w: int) -> None:
        r = self.hk.send(self.hk.root_id, {"type": "update_agent", "id": AI, "context_window": w})
        if r.get("updated") is not True:
            self.need(False, f"the chat's window set to {w}: {str(r)[:200]}")

    def history(self) -> list[dict]:
        return self.pages({})[0]

    def fold_ids(self) -> list[int]:
        """The folds in the chat's view now: the ids its last projection shows
        (`context_status`) that `history {fold}` opens as a fold (a fold has no chat role,
        so the plain `history` lists only turns)."""
        cs = self.hk.send(AI, {"type": "context_status"})
        view = (cs.get("last_projection") or {}).get("view_ids") or []
        return [i for i in view if isinstance(i, int)
                and (self.hk.send(AI, {"type": "history", "fold": i, "limit": 1}).get("fold")
                     or {}).get("kind") == "fold"]

    def fold(self, fid: int) -> dict:
        """`{id, title, summary, members: [{id, role, content}]}` of one fold."""
        rows, h = self.pages({"fold": fid})
        head = h.get("fold") or {}
        members = []
        for r in rows:
            got = self.hk.send(AI, {"type": "recall", "id": r["id"]})
            ms = got.get("messages") or [{}]
            members.append({"id": r["id"], "role": r.get("role"),
                            "kind": r.get("kind"), "content": str(ms[0].get("content") or "")})
        return {"id": fid, "title": head.get("title"), "summary": head.get("summary") or "",
                "members": members, "refs": head.get("members")}

    def until_fold(self, label: str, known: set, step, limit: int = 40) -> int | None:
        """Turns `step(k)` (k = 0, 1, …) until a fold not in `known` lands; its id."""
        for k in range(limit):
            text, reply = step(k)
            self.turn(text, reply)
            fresh = [f for f in self.fold_ids() if f not in known]
            if fresh:
                print(f"  info {label}: a fold after {k + 1} turns: {fresh}", flush=True)
                known.update(fresh)
                return fresh[0]
        rows = self.history()
        sizes = [len(blob(t)) for t in self.mock.turns()][-3:]
        print(f"  info {label}: {len(rows)} rows, {len(self.mock.turns())} requests, "
              f"last request sizes {sizes}, context {str(self.hk.send(AI, {'type': 'context_status'}))[:600]}, "
              f"usage {[(t.get('body') or {}).get('stream_options') for t in self.mock.turns()[-1:]]}",
              flush=True)
        self.need(False, f"{label}: a fold within {limit} turns")
        return None

    @staticmethod
    def last_line(members: list[dict]) -> str | None:
        """The latest line the stretch's answers write, by member id."""
        out = None
        for m in sorted(members, key=lambda m: m["id"]):
            if m["role"] == "assistant":
                found = LINE_RE.findall(m["content"])
                if found:
                    out = " ".join(found[-1].split())
        return out

    def reflect_line(self) -> dict:
        return self.hk.send(AI, {"type": "reflect"}).get("line_summary") or {}

    def close(self) -> None:
        self.hk.close()
        self.mock.stop()


def filler(tag: str) -> str:
    """A person's message of FILLER characters, distinct per tag."""
    words = []
    i = 0
    while sum(len(w) + 1 for w in words) < FILLER:
        words.append(f"{tag}w{i}")
        i += 1
    return " ".join(words)


def blob(req: dict) -> str:
    return json.dumps(messages(req), ensure_ascii=False)
