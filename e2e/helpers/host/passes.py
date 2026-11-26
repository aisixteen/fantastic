"""ONE PASS OF AN AI AGENT, from the wire (#795).

A `send` to an AI agent is acked once its message is stored (`{accepted, re}`); the
answer is the agent's own row, written when its pass ends. `run_pass` sends, follows the
agent's `status` events over the ws face until the pass ends (`done` or `failed`), and
reads what the pass left: `response` (the answer row's text) or `error` (the `failed`
status's reason).
"""

from __future__ import annotations

import asyncio
import json
import re
import threading
import time

from helpers.host.ws import WSWatcher


def _watch(port: int, aid: str) -> tuple[list, threading.Event]:
    seen: list = []
    stop = threading.Event()
    ready = threading.Event()

    async def run():
        w = WSWatcher(port, aid)
        await w.connect()
        ready.set()

        async def pump():
            async for ev in w.events():
                seen.append(ev)

        reader = asyncio.ensure_future(pump())
        try:
            while not stop.is_set() and not reader.done():
                await asyncio.sleep(0.05)
        finally:
            reader.cancel()
            await w.close()

    threading.Thread(target=lambda: asyncio.run(run()), daemon=True).start()
    ready.wait(10)
    return seen, stop


def _ended(events: list) -> dict | None:
    return next((e for e in list(events)
                 if e.get("type") == "status" and e.get("phase") in ("done", "failed")), None)


def run_pass(hk, aid: str, payload: dict, bound: float = 60) -> dict:
    """`payload` sent to `aid`; its pass followed to its end. `{ack, response, error,
    events}`: `events` are the agent's events up to that end."""
    events, stop = _watch(hk.port, aid)
    # the socket is open; the kernel registers its watch a beat later, and a pass that
    # fails at once (a record it cannot run on) emits its `failed` within that beat
    time.sleep(0.3)
    try:
        ack = hk.send(aid, payload)
        end = time.monotonic() + bound
        while time.monotonic() < end and _ended(events) is None:
            time.sleep(0.05)
    finally:
        stop.set()
    seen = list(events)
    last = _ended(seen)
    upto = seen.index(last) + 1 if last is not None else len(seen)
    out: dict = {"ack": ack, "response": None, "error": None, "events": seen[:upto]}
    if not isinstance(ack, dict) or ack.get("accepted") is not True:
        out["error"] = json.dumps(ack)
        return out
    if last is None:
        out["error"] = f"the pass did not end in {bound}s"
    elif last.get("phase") == "failed":
        out["error"] = json.dumps(last.get("detail") or last)
    re_ = ack.get("re") or 0
    rows = (hk.send(aid, {"type": "history", "limit": 500}) or {}).get("rows") or []
    answers = [r for r in rows if isinstance(r, dict) and r.get("role") == "assistant"
               and (r.get("id") or 0) > re_]
    if answers:
        out["response"] = str(answers[-1].get("content") or answers[-1].get("head") or "")
    return out


#: A scripted model's answer while a batch it sent is not whole yet: nothing to do now.
WAIT = '<call name="done"></call>'

_TAG = re.compile(r"\[reply to [^\]]*? (\d+) of (\d+) in batch (\S+?)\]")


def _content(m: dict) -> str:
    c = m.get("content")
    return c if isinstance(c, str) else json.dumps(c)


def since_answer(req: dict) -> list[str]:
    """The user messages a request carries after its last SCRIPTED answer (a `WAIT`
    is not one): every reply that answer's calls brought, in the order stored."""
    msgs = req.get("messages") or []
    at = max((i for i, m in enumerate(msgs)
              if m.get("role") == "assistant" and _content(m).strip() != WAIT), default=-1)
    return [_content(m) for m in msgs[at + 1:] if m.get("role") == "user"]


def batch_whole(req: dict) -> bool:
    """Every batch the request's replies belong to is whole: each reply is its own
    message (`k of M in batch B`, #795), and the first one wakes a pass."""
    seen: dict[str, tuple[set, int]] = {}
    for u in since_answer(req):
        for k, m, b in _TAG.findall(u):
            ks, _ = seen.setdefault(b, (set(), int(m)))
            ks.add(int(k))
    return all(len(ks) >= m for ks, m in seen.values())


def scripted(script: list[str]):
    """A responder answering `script` in order, ONE answer per whole set of replies: a
    pass woken while a batch is still arriving answers `WAIT`. `(respond, served)`:
    `served` are the requests a scripted answer went to, in order."""
    queue = list(script)
    served: list[dict] = []
    lock = threading.Lock()

    def respond(req: dict) -> str | None:
        with lock:
            if served and not batch_whole(req):
                return WAIT
            if not queue:
                return None
            served.append(req)
            return queue.pop(0)

    return respond, served
