"""kernel/foreign_cut_give_up — the call checker on the STREAM (#499), against a
SCRIPTED model. NO MODEL.

A local OpenAI-compatible server plays the model (two: `m` for the AI, `mb` for the
BOSS it hands its text to); the kernel's own `vllm_backend` agents (ai_core) on the
PACKAGED host of `RUNTIME` (python | rust) run the chains, driven through `web_rest`.

  A. the model writes a foreign call tag (`<invoke name="send">` followed by a loop of
     closers) turn after turn: each answer is CUT at the tag, the correction
     (`[malformed call — nothing ran]`) is the next request, the loop is never stored,
     and after exactly 9 cut turns in a row the chain gives up — no 10th request;
  B. 8 cut turns, a real call, 8 cut turns, done: the call resets the count — 18
     requests, no give-up;
  C. a cut after thinking: the thought is kept in the store (`has_thinking`) and is
     not in the next request.
"""

from __future__ import annotations

import json
import os
import secrets
import shutil
import sys
import tempfile
import time
import uuid
from pathlib import Path

E2E = next(d for d in Path(__file__).resolve().parents if (d / ".e2e-root").exists())
sys.path.insert(0, str(E2E))

from helpers.host.driver import HostKernel, ensure_runtime, runtime
from helpers.verdict import verdict
from mocks import halves, sse
from mocks.vllm import VllmMock

REPO = E2E.parent
RUNTIME = os.environ.get("RUNTIME", "")
AI, BOSS = "ai", "boss"
MODEL = "mock-scripted"
TOKEN = "E138-" + secrets.token_hex(4)
FOREIGN = 'Sending it.\n<invoke name="send">\n' + '</invoke>\n' * 400
DONE = '<call name="done"></call>'
GAVE_UP = ("The LLM was not able to answer — 9 turns in a row were cut at a foreign "
           "call syntax. Reiterate the inquiry.")


class ThinkingVllm(VllmMock):
    """The vllm mock, plus vLLM's thought channel: a scripted answer
    `<think>T</think>A` streams T as `reasoning_content` deltas, then A as
    `content` — the shape a reasoning model's server sends."""

    def turn(self, h, req, text):
        think, answer = "", text
        if text.startswith("<think>") and "</think>" in text:
            think, answer = text[len("<think>"):].split("</think>", 1)
        cid = "chatcmpl-" + uuid.uuid4().hex
        created = int(time.time())

        def chunk(delta, finish=None):
            return sse(None, {"id": cid, "object": "chat.completion.chunk",
                              "created": created, "model": self.model,
                              "choices": [{"index": 0, "delta": delta, "logprobs": None,
                                           "finish_reason": finish}]})

        out = [chunk({"role": "assistant", "content": ""})]
        out += [chunk({"reasoning_content": p}) for p in halves(think)]
        out += [chunk({"content": p}) for p in halves(answer)]
        out.append(chunk({}, "stop"))
        if (req.get("stream_options") or {}).get("include_usage"):
            p, c = self.usage(text)
            out.append(sse(None, {"id": cid, "object": "chat.completion.chunk",
                                  "created": created, "model": self.model, "choices": [],
                                  "usage": {"prompt_tokens": p, "completion_tokens": c,
                                            "total_tokens": p + c}}))
        out.append(b"data: [DONE]\n\n")
        h.stream("text/event-stream; charset=utf-8", out)


def last_user(req: dict) -> str:
    """The last user message of a chat request, as text."""
    for m in reversed(req.get("messages") or []):
        if m.get("role") == "user":
            c = m.get("content")
            return c if isinstance(c, str) else json.dumps(c)
    return ""


def last_user(req: dict) -> str:
    """The last user message of a chat request, as text."""
    for m in reversed(req.get("messages") or []):
        if m.get("role") == "user":
            c = m.get("content")
            return c if isinstance(c, str) else json.dumps(c)
    return ""


def main() -> int:
    name, _ = runtime(REPO, RUNTIME)
    spec = ensure_runtime(REPO, name)
    m = ThinkingVllm(model=MODEL).start()
    mb = ThinkingVllm(model=MODEL).start()
    work = Path(tempfile.mkdtemp(prefix="e2e-foreign-cut-"))
    hk = HostKernel(spec, work, reach=[AI, BOSS]).boot()
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

    def turns_at_least(mock, n: int, bound: float = 90) -> list[dict]:
        end = time.monotonic() + bound
        while time.monotonic() < end:
            got = mock.turns()
            if len(got) >= n:
                return got
            time.sleep(0.05)
        return mock.turns()

    def idle(bound: float = 90) -> bool:
        """Poll (bounded) until neither agent has a task in flight (`ps`)."""
        end = time.monotonic() + bound
        while time.monotonic() < end:
            tasks = hk.send("kernel", {"type": "ps"}).get("tasks") or []
            if not any(t.get("agent") in (AI, BOSS) for t in tasks):
                return True
            time.sleep(0.05)
        return False

    def history() -> list[dict]:
        h = hk.send(AI, {"type": "history", "limit": 500})
        return h.get("rows") or h.get("history") or h.get("messages") or []

    reflect = (f'Looking around.\n<call name="send">\n<routing><target_id>{root}</target_id>'
               '</routing>\n<payload><type>reflect</type>\n<tree>none</tree></payload>\n</call>')
    try:
        r = hk.send(root, {"type": "update_agent", "id": "memory",
                           "callable_by": {**callable_by("memory"), AI: "*", BOSS: "*"}})
        need(r.get("updated") is True, f"memory accepts both agents: {str(r)[:200]}")
        for aid, mock, can in (
            (AI, m, {"$self": "*", "memory": "*", root: ["reflect"], BOSS: ["send"]}),
            (BOSS, mb, {"$self": "*", "memory": "*"}),
        ):
            by = {"$self": "*", "$parent": "*", "rest": "*"}
            if aid == BOSS:
                by[AI] = ["send"]
            rec = hk.create(parent_id="rest", handler_module="vllm_backend.tools", id=aid,
                            endpoint=mock.url, model=MODEL, max_tokens=8192,
                            context_window=262144, kv_slots=4, kv_overflow="evict",
                            memory_store_id="memory", can_call=can, callable_by=by)
            need(rec.get("id") == aid, f"create {aid}: {str(rec)[:200]}")

        # ── A · nine cut turns in a row give up ───────────────────────────────
        m.reply(*[FOREIGN] * 12)
        mb.reply(*[DONE] * 12)
        out = hk.send(AI, {"type": "send", "text": "go A", "send_to": BOSS})
        need(not out.get("error"), f"the send is accepted: {str(out)[:200]}")
        turns_at_least(m, 9)
        need(idle(), "A: the chains settle")
        time.sleep(1)
        turns = m.turns()
        need(len(turns) == 9, f"A: exactly 9 requests, none after the give-up: {len(turns)}")
        need(all("[malformed call — nothing ran]" in last_user(t["body"]) for t in turns[1:9]),
             "A: requests 2-9 each carry the malformed-call correction")
        rows = [r for r in history() if r.get("role") == "assistant"]
        need(bool(rows) and all(int(r.get("bytes") or 0) < 60 for r in rows),
             f"A: the loop was never stored: {[r.get('bytes') for r in rows]}")

        # ── B · 8 cuts, a call, 8 cuts, done: no give-up ──────────────────────
        m.clear()
        mb.clear()
        m.reply(*[FOREIGN] * 8, reflect, *[FOREIGN] * 8, DONE)
        out = hk.send(AI, {"type": "send", "text": "go B"})
        need(not out.get("error"), f"B: the send is accepted: {str(out)[:200]}")
        turns_at_least(m, 18)
        need(idle(), "B: the chain settles")
        time.sleep(1)
        need(len(m.turns()) == 18, f"B: exactly 18 requests: {len(m.turns())}")
        need(not any("were cut at a foreign call syntax" in json.dumps(t["body"])
                     for t in mb.turns()),
             "B: nobody was told of a give-up")

        # ── C · a cut after thinking: stored, not re-sent ─────────────────────
        m.clear()
        mb.clear()
        m.reply(f'<think>PLAN-{TOKEN} then invoke</think>Sending it.\n<invoke name="send">',
                DONE)
        out = hk.send(AI, {"type": "send", "text": "go C"})
        need(not out.get("error"), f"C: the send is accepted: {str(out)[:200]}")
        turns_at_least(m, 2)
        need(idle(), "C: the chain settles")
        turns = m.turns()
        need(len(turns) == 2, f"C: two requests: {len(turns)}")
        if len(turns) > 1:
            msgs = json.dumps(turns[1]["body"].get("messages") or [])
            need(TOKEN not in msgs and "Sending it." in msgs,
                 "C: request 2 holds the cut answer, not the thought")
        need(any(r.get("has_thinking") and "Sending it." in str(r.get("head"))
                 for r in history() if r.get("role") == "assistant"),
             "C: the cut assistant row keeps its thinking in the store")
    finally:
        hk.close()
        m.stop()
        mb.stop()
        shutil.rmtree(work, ignore_errors=True)

    verdict(not bad, f"{RUNTIME}: {len(bad)} failed" + (f" — first: {bad[0]}" if bad else ""))
    return 0 if not bad else 1


if __name__ == "__main__":
    sys.exit(main())
