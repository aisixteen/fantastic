"""kernel/failed_pass_retry — a failed pass is retried only by `transport_retries`,
inside the pass (#533, #795). NO MODEL.

A scripted OpenAI-compatible server fails the way a provider does, for the kernel's
own `vllm_backend` agent (ai_core) on the PACKAGED rust host; the chat has
`transport_retries: 1`:

  1. an HTTP 500 error reply, then a plain `done`: request 2 is the same pass retried
     on the link, it answers, and the history holds no `error` row; the chain settles;
  2. a stream cut at `max_tokens` (`finish_reason: length`): no link failure, so one
     request; the history holds an `error` row naming `max_tokens`; the chain settles;
  3. HTTP 500 on every request: exactly 1 + transport_retries (2) requests, then the
     pass fails — the agent goes idle and no further request arrives.
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
from helpers.lines import ALL, PROVIDER
from helpers.verdict import verdict
from mocks import DONE
from mocks.vllm import VllmMock

REPO = E2E.parent
RUNTIME = os.environ.get("RUNTIME", "")
AI, PROV = "ai", "prov"
MODEL = "mock-failing"
RETRIES = 1
CUT = "<cut>"


class CuttingVllm(VllmMock):
    """The vllm mock, plus a stream cut at the output bound: an answer starting with
    `<cut>` streams the rest, then ends with `finish_reason: length`."""

    def on_pass(self, h, req, text):
        if not text.startswith(CUT):
            return super().on_pass(h, req, text)
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
    work = Path(tempfile.mkdtemp(prefix="e2e-failed-pass-"))
    hk = HostKernel(spec, work).boot()
    root = hk.root_id
    bad: list[str] = []

    def need(cond: bool, what: str) -> None:
        print(f"  {'ok ' if cond else 'BAD'} {what}", flush=True)
        if not cond:
            bad.append(what)

    def passes_at_least(n: int, bound: float = 60) -> list[dict]:
        end = time.monotonic() + bound
        while time.monotonic() < end:
            if len(m.passes()) >= n:
                break
            time.sleep(0.05)
        return m.passes()

    def idle(bound: float = 60) -> bool:
        end = time.monotonic() + bound
        while time.monotonic() < end:
            if hk.quiet(AI):
                return True
            time.sleep(0.05)
        return False

    def error_rows() -> list[dict]:
        r = hk.send(AI, {"type": "history", "limit": 500})
        return [x for x in r.get("rows") or [] if x.get("kind") == "error"]

    def say(text: str) -> None:
        out = hk.send(AI, {"type": "send", "text": text})
        # a failed pass's reply may carry the error; only a refusal of the send fails
        need("chat's verb" not in str(out), f"the send reached the chat: {str(out)[:160]}")

    try:
        # a provider (the vllm_backend) and the chat (ai_chat.tools) naming it
        rec = hk.create(parent_id="rest", handler_module="vllm_backend.tools", id=PROV,
                        endpoint=m.url, model=MODEL,
                        context_window=262144, kv_slots=4, kv_overflow="evict",
                        memory_store_id="memory", can={"memory": ALL})
        need(rec.get("id") == PROV, f"create {PROV}: {str(rec)[:200]}")
        rec = hk.create(parent_id="rest", handler_module="ai_chat.tools", id=AI,
                        provider_id=PROV, model=MODEL, max_tokens=8192,
                        transport_retries=RETRIES,
                        context_window=262144, memory_store_id="memory",
                        can={"memory": ALL, PROV: PROVIDER})
        need(rec.get("id") == AI, f"create {AI}: {str(rec)[:200]}")

        # ── 1 · an error reply, retried on the link inside the pass ──────
        m.error(500, times=1).reply("Recovered.\n" + DONE)
        say("first")
        passes_at_least(2)
        need(idle(), "1: the chain settles")
        time.sleep(0.5)
        passes = m.passes()
        need(len(passes) == 2, f"1: the failed request and its link retry: {len(passes)} requests")
        need(bool(passes) and passes[0]["answered"] == 500, "1: the first request was answered with the error")
        need(not error_rows(), f"1: the retried pass answered, no error row: {error_rows()}")

        # ── 2 · a stream cut at max_tokens ────────────────────────────────
        m.clear()
        m.reply(CUT + "a long answer that never finishes", "Recovered.\n" + DONE)
        say("second")
        passes_at_least(1)
        need(idle(), "2: the chain settles")
        time.sleep(1.0)
        passes = m.passes()
        need(len(passes) == 1, f"2: a cut is no link failure, one request: {len(passes)} requests")
        rows = error_rows()
        need(len(rows) == 1 and "max_tokens" in str(rows[0]),
             f"2: one error row naming max_tokens: {rows}")

        # ── 3 · failures on every request end the pass ────────────────────
        m.clear()
        m.error(500, times=None)
        say("third")
        passes_at_least(1 + RETRIES)
        need(idle(), "3: the pass ends (the agent has no task in `ps`)")
        time.sleep(1.0)
        n = len(m.passes())
        need(n == 1 + RETRIES,
             f"3: exactly 1 + transport_retries ({1 + RETRIES}) requests, no hang: {n}")
        m.error(None)
        need(idle(5) and len(m.passes()) == n, "3: no request follows (the pass failed)")
        need(len(error_rows()) == 2, f"3: the failed pass wrote its error row: {len(error_rows())}")
    finally:
        hk.close()
        m.stop()
        shutil.rmtree(work, ignore_errors=True)

    verdict(not bad, f"{RUNTIME}: {len(bad)} failed" + (f" — first: {bad[0]}" if bad else ""))
    return 0 if not bad else 1


if __name__ == "__main__":
    sys.exit(main())
