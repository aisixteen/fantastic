"""kernel/failed_turn_retry — a failed turn is retried as the chain's own next turn
(#533). NO MODEL.

A scripted OpenAI-compatible server fails the way a provider does, for the kernel's
own `vllm_backend` agent (ai_core) on the PACKAGED host of RUNTIME (python | rust):

  1. an HTTP 500 error reply, then a plain `done`: request 2 is the agent's own next
     turn, its last user turn carries `[turn failed]` and the error; the history
     holds one `error` row; the chain settles;
  2. a stream cut at `max_tokens` (`finish_reason: length`), then `done`: the same,
     and the text names `max_tokens`;
  3. HTTP 500 on every turn: exactly MAX_SILENT_TURNS (10) requests, then the chain
     stops (`gave_up`) — the agent goes idle and no further request arrives.
"""

from __future__ import annotations

import json
import os
import shutil
import sys
import tempfile
import time
from pathlib import Path

E2E = next(d for d in Path(__file__).resolve().parents if (d / ".e2e-root").exists())
sys.path.insert(0, str(E2E))

from helpers.host.driver import HostKernel, ensure_runtime, runtime
from helpers.verdict import verdict
from mocks import DONE
from mocks.vllm import VllmMock

REPO = E2E.parent
RUNTIME = os.environ.get("RUNTIME", "")
AI, PROV = "ai", "prov"
MODEL = "mock-failing"
MAX_SILENT_TURNS = 10
CUT = "<cut>"


class CuttingVllm(VllmMock):
    """The vllm mock, plus a stream cut at the output bound: an answer starting with
    `<cut>` streams the rest, then ends with `finish_reason: length`."""

    def turn(self, h, req, text):
        if not text.startswith(CUT):
            return super().turn(h, req, text)
        from mocks import halves, sse

        def chunk(delta, finish=None):
            return sse(None, {"id": "chatcmpl-cut", "object": "chat.completion.chunk",
                              "created": 0, "model": self.model,
                              "choices": [{"index": 0, "delta": delta, "logprobs": None,
                                           "finish_reason": finish}]})

        out = [chunk({"role": "assistant", "content": ""})]
        out += [chunk({"content": p}) for p in halves(text[len(CUT):])]
        out.append(chunk({}, "length"))
        out.append(b"data: [DONE]\n\n")
        h.stream("text/event-stream; charset=utf-8", out)


def last_user(req: dict) -> str:
    for m in reversed(req.get("messages") or []):
        if m.get("role") == "user":
            c = m.get("content")
            return c if isinstance(c, str) else json.dumps(c)
    return ""


def main() -> int:
    name, _ = runtime(REPO, RUNTIME)
    spec = ensure_runtime(REPO, name)
    m = CuttingVllm(model=MODEL).start()
    work = Path(tempfile.mkdtemp(prefix="e2e-failed-turn-"))
    hk = HostKernel(spec, work, reach=[AI, PROV]).boot()
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

    def turns_at_least(n: int, bound: float = 60) -> list[dict]:
        end = time.monotonic() + bound
        while time.monotonic() < end:
            if len(m.turns()) >= n:
                break
            time.sleep(0.05)
        return m.turns()

    def idle(bound: float = 60) -> bool:
        end = time.monotonic() + bound
        while time.monotonic() < end:
            tasks = hk.send("kernel", {"type": "ps"}).get("tasks") or []
            if not any(t.get("agent") == AI for t in tasks):
                return True
            time.sleep(0.05)
        return False

    def error_rows() -> list[dict]:
        r = hk.send(AI, {"type": "history", "limit": 500})
        return [x for x in r.get("rows") or [] if x.get("kind") == "error"]

    def say(text: str) -> None:
        out = hk.send(AI, {"type": "send", "text": text})
        # a failed turn's reply may carry the error; only a refusal of the send fails
        need("chat's verb" not in str(out), f"the send reached the chat: {str(out)[:160]}")

    try:
        r = hk.send(root, {"type": "update_agent", "id": "memory",
                           "callable_by": {**callable_by("memory"), AI: "*", PROV: "*"}})
        need(r.get("updated") is True, f"memory accepts the AI: {str(r)[:200]}")
        # a provider (the vllm_backend) and the chat (ai_chat.tools) naming it
        rec = hk.create(parent_id="rest", handler_module="vllm_backend.tools", id=PROV,
                        endpoint=m.url, model=MODEL, max_tokens=8192,
                        context_window=262144, kv_slots=4, kv_overflow="evict",
                        memory_store_id="memory", can_call={"$self": "*", "memory": "*"},
                        callable_by={"$self": "*", "$parent": "*", AI: "*"})
        need(rec.get("id") == PROV, f"create {PROV}: {str(rec)[:200]}")
        rec = hk.create(parent_id="rest", handler_module="ai_chat.tools", id=AI,
                        provider_id=PROV, model=MODEL, max_tokens=8192,
                        context_window=262144, memory_store_id="memory",
                        can_call={"$self": "*", "memory": "*", PROV: ["complete", "watch", "reflect"]},
                        callable_by={"$self": "*", "$parent": "*", "rest": "*"})
        need(rec.get("id") == AI, f"create {AI}: {str(rec)[:200]}")

        # ── 1 · an error reply ────────────────────────────────────────────
        m.error(500, times=1).reply("Recovered.\n" + DONE)
        say("first")
        turns = turns_at_least(2)
        need(idle(), "1: the chain settles")
        turns = m.turns()
        need(len(turns) == 2, f"1: the failed turn and its own retry: {len(turns)} requests")
        u = last_user(turns[1]["body"]) if len(turns) > 1 else ""
        need("[turn failed]" in u, f"1: the next request carries [turn failed]: {u[:200]!r}")
        need(bool(turns) and turns[0]["answered"] == 500, "1: the first request was answered with the error")
        need(len(error_rows()) == 1, f"1: the history holds one error row: {error_rows()}")

        # ── 2 · a stream cut at max_tokens ────────────────────────────────
        m.clear()
        m.reply(CUT + "a long answer that never finishes", "Recovered.\n" + DONE)
        say("second")
        turns_at_least(2)
        need(idle(), "2: the chain settles")
        turns = m.turns()
        need(len(turns) == 2, f"2: the cut turn and its own retry: {len(turns)} requests")
        u = last_user(turns[1]["body"]) if len(turns) > 1 else ""
        need("[turn failed]" in u and "max_tokens" in u,
             f"2: the next request carries [turn failed] and max_tokens: {u[:240]!r}")
        need(len(error_rows()) == 2, f"2: a second error row: {len(error_rows())}")

        # ── 3 · failures in a row end the chain ───────────────────────────
        m.clear()
        m.error(500, times=None)
        say("third")
        turns_at_least(MAX_SILENT_TURNS)
        need(idle(), "3: the chain ends (the agent has no task in `ps`)")
        time.sleep(1.0)
        n = len(m.turns())
        need(n == MAX_SILENT_TURNS,
             f"3: exactly MAX_SILENT_TURNS ({MAX_SILENT_TURNS}) requests, no hang: {n}")
        m.error(None)
        need(idle(5) and len(m.turns()) == n, "3: no request follows (gave_up)")
        print(f"  ── last request of the run: {last_user(m.turns()[-1]['body'])[:200]!r}"
              if m.turns() else "  (no requests)", flush=True)
    finally:
        hk.close()
        m.stop()
        shutil.rmtree(work, ignore_errors=True)

    verdict(not bad, f"{RUNTIME}: {len(bad)} failed" + (f" — first: {bad[0]}" if bad else ""))
    return 0 if not bad else 1


if __name__ == "__main__":
    sys.exit(main())
