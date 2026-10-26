"""backends/transport_retries — a dropped link is retried visibly, then the turn works. NO MODEL.

The vllm mock (`e2e/mocks/vllm.py`) answers HTTP 503, and the kernel's own `vllm_backend`
agent on the PACKAGED host of `RUNTIME` (python), every call through `web_rest`, retries:

  A. one 503, then an answer: two requests, the second carries the `[retry 1/2]` notice
     row, a `retrying` status (attempt 1 of 2, the error naming 503) is emitted, the
     notice is stored, the planted token is the final answer, the agent ends idle;
  B. a 503 on every try with `transport_retries: 1`: exactly two requests, the turn fails
     loud naming 503, one `retrying` status;
  C. a 400 (a model-side refusal) and `transport_retries: 0` on a 503: one request each,
     no `retrying` status.
"""

from __future__ import annotations

import asyncio
import json
import os
import secrets
import shutil
import sys
import tempfile
import threading
import time
from pathlib import Path

E2E = next(d for d in Path(__file__).resolve().parents if (d / ".e2e-root").exists())
sys.path.insert(0, str(E2E))

from helpers.host.driver import HostKernel, ensure_runtime, runtime
from helpers.host.ws import WSWatcher
from helpers.verdict import verdict
from mocks.vllm import VllmMock

REPO = E2E.parent
RUNTIME = os.environ.get("RUNTIME", "")
AI = "ai"
MODEL = "mock-retry"
DONE = '<call name="done"></call>'


def watch(port: int, aid: str) -> tuple[list, threading.Event]:
    """Collect `aid`'s events over the ws face in a thread until `stop` is set."""
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

        # one reader task, never under a timeout (a timed-out wait closes the generator)
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


def main() -> int:
    name, _ = runtime(REPO, RUNTIME)
    spec = ensure_runtime(REPO, name)
    m = VllmMock(model=MODEL).start()
    work = Path(tempfile.mkdtemp(prefix="e2e-transport-retries-"))
    hk = HostKernel(spec, work, reach=[AI]).boot()
    root = hk.root_id
    bad: list[str] = []

    def need(cond: bool, what: str) -> None:
        print(f"  {'ok ' if cond else 'BAD'} {what}", flush=True)
        if not cond:
            bad.append(what)

    def callable_by(aid: str) -> dict:
        r = hk.send("kernel", {"type": "list_agents"})
        rec = next(a for a in r.get("agents") or [] if a.get("id") == aid)
        return dict(rec.get("callable_by") or {})

    def retrying(events: list) -> list[dict]:
        return [e for e in list(events) if e.get("type") == "status" and e.get("phase") == "retrying"]

    def run_turn(text: str):
        """One send with the ws collector on; the done status is waited for."""
        events, stop = watch(hk.port, AI)
        before = len(m.turns())
        out = hk.send(AI, {"type": "send", "text": text, "client_id": "tab1"})
        end = time.monotonic() + 15
        while time.monotonic() < end and not any(
            e.get("type") == "status" and e.get("phase") == "done" for e in list(events)
        ):
            time.sleep(0.05)
        stop.set()
        return out, events, m.turns()[before:]

    def idle(bound: float = 30) -> bool:
        end = time.monotonic() + bound
        while time.monotonic() < end:
            tasks = hk.send("kernel", {"type": "ps"}).get("tasks") or []
            if not any(t.get("agent") == AI for t in tasks):
                return True
            time.sleep(0.05)
        return False

    def notices() -> list[str]:
        h = hk.send(AI, {"type": "history"})
        rows = h.get("rows") or h.get("history") or h.get("messages") or []
        return [str(r.get("head") or r.get("content")) for r in rows
                if isinstance(r, dict) and r.get("kind") == "notice"
                and "[retry " in str(r.get("head") or r.get("content"))]

    try:
        r = hk.send(root, {"type": "update_agent", "id": "memory",
                           "callable_by": {**callable_by("memory"), AI: "*"}})
        need(r.get("updated") is True, f"memory accepts the AI: {str(r)[:200]}")
        rec = hk.create(parent_id="rest", handler_module="vllm_backend.tools", id=AI,
                        endpoint=m.url, model=MODEL, max_tokens=8192,
                        context_window=262144, kv_slots=4, kv_overflow="evict",
                        transport_retries=2, memory_store_id="memory",
                        can_call={"$self": "*", "memory": "*"},
                        callable_by={"$self": "*", "$parent": "*", "rest": "*", "ws": "*"})
        need(rec.get("id") == AI, f"create {AI}: {str(rec)[:200]}")

        # A · one 503, then an answer
        token = "ANSWER-" + secrets.token_hex(4)
        m.error(503, times=1)
        m.reply(f"{token} {DONE}")
        out, events, turns = run_turn("hello")
        need(not out.get("error"), f"A: the send succeeds after the retry: {str(out)[:200]}")
        need([t["answered"] for t in turns] == [503, 200], f"A: 503 then 200: {[t['answered'] for t in turns]}")
        second = json.dumps((turns[1]["body"] if len(turns) > 1 else {}).get("messages") or [])
        first = json.dumps((turns[0]["body"] if turns else {}).get("messages") or [])
        need("[retry 1/2]" in second and "[retry 1/2]" not in first,
             "A: the second request carries the [retry 1/2] notice row, the first does not")
        ret = retrying(events)
        d = (ret[0].get("detail") or {}) if ret else {}
        need(len(ret) == 1 and d.get("attempt") == 1 and d.get("of") == 2 and "503" in str(d.get("error")),
             f"A: one retrying status, attempt 1 of 2, naming 503: {d}")
        need(token in str(out.get("response")), f"A: the final answer carries {token}")
        need(idle(), "A: the agent ends idle (no task left in ps)")
        need(any("[retry 1/2]" in n for n in notices()), f"A: the notice row is stored: {notices()}")

        # B · a 503 on every try, one retry allowed
        hk.send(root, {"type": "update_agent", "id": AI, "transport_retries": 1})
        m.error(503, times=None)
        out, events, turns = run_turn("again")
        m.error(None)
        need(bool(out.get("error")) and "503" in json.dumps(out),
             f"B: the turn fails loud naming 503: {str(out)[:200]}")
        need(len(turns) == 2, f"B: exactly 2 requests (1 try + 1 retry): {len(turns)}")
        need(len(retrying(events)) == 1, f"B: exactly one retrying status: {len(retrying(events))}")
        need(idle(), "B: the agent ends idle")

        # C · no retry for a 400, nor with retries off
        hk.send(root, {"type": "update_agent", "id": AI, "transport_retries": 2})
        m.error(400, times=None)
        out, events, turns = run_turn("bad request")
        m.error(None)
        need(bool(out.get("error")) and len(turns) == 1 and not retrying(events),
             f"C: a 400 is not retried: {len(turns)} request(s), {len(retrying(events))} retrying")
        hk.send(root, {"type": "update_agent", "id": AI, "transport_retries": 0})
        m.error(503, times=None)
        out, events, turns = run_turn("no retries")
        m.error(None)
        need(bool(out.get("error")) and len(turns) == 1 and not retrying(events),
             f"C: transport_retries 0 → a 503 is not retried: {len(turns)} request(s)")
    finally:
        hk.close()
        m.stop()
        shutil.rmtree(work, ignore_errors=True)

    verdict(not bad, f"{RUNTIME}: {len(bad)} failed" + (f" — first: {bad[0]}" if bad else ""))
    return 0 if not bad else 1


if __name__ == "__main__":
    sys.exit(main())
