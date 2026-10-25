"""kernel/scripted_ai — an AI agent's call loop against a SCRIPTED model. NO MODEL.

A local OpenAI-compatible server (`e2e/mocks/vllm.py`, extended here with vLLM's
thought channel `reasoning_content`) plays the model; the kernel's own
`vllm_backend` agent (ai_core) on the PACKAGED host of `RUNTIME` (python |
rust) runs the chain, every call from the test through `web_rest`. Tool calling
is the prompt-taught `<call name>` tag grammar — the request carries no `tools`.
Every field the backend needs is SET on the record (endpoint, model, max_tokens,
context_window, kv_slots, kv_overflow), so nothing is read from the server.

The script, one model answer per request:

  1. `<call name="send">` → `reflect` on the root: dispatched; its result comes
     back as the NEXT request's last user turn, `[reply to <pid> · kernel_state
     reflect]` carrying the root's guid;
  2. a `write` to a file_bridge, `bytes` verbatim: the file lands on disk with
     exactly those bytes; the next request carries the `write` reply;
  3. a call written WRONG — a `<tool_call><function=call name="send">` wrapper and
     an envelope with text outside a tag: nothing runs (no file); the next request
     carries the compiler-style report (`[malformed call — nothing ran] line L,
     col C`, a caret, `write it as:` + the form) and the envelope's fault (`call
     NOT dispatched: text outside a tag`);
  4. a call INSIDE the thought (`reasoning_content`), an answer with none: nothing
     is dispatched (no file); the next request carries the no-call reminder
     (`[no call was dispatched]`), and its history holds the answer without the
     thought (stripped before the turn is stored);
  5. a plain answer + `done`: the chain ends — no further request reaches the
     server once the agent has nothing in flight.

Across the run: every request is a streamed chat completion naming the record's
model, with no `tools` array; the backend made no `/models` or `/metrics` probe.
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
AI, FILES = "ai", "files"
MODEL = "mock-scripted"
TOKEN = "E119-" + secrets.token_hex(4)


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


def main() -> int:
    name, _ = runtime(REPO, RUNTIME)
    spec = ensure_runtime(REPO, name)
    m = ThinkingVllm(model=MODEL).start()
    work = Path(tempfile.mkdtemp(prefix="e2e-scripted-ai-"))
    (work / "files").mkdir()
    hk = HostKernel(spec, work, reach=[AI, FILES]).boot()
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
        """Poll (bounded) until the server has seen `n` turn requests."""
        end = time.monotonic() + bound
        while time.monotonic() < end:
            got = m.turns()
            if len(got) >= n:
                return got
            time.sleep(0.05)
        return m.turns()

    def idle(bound: float = 60) -> bool:
        """Poll (bounded) until the AI has no task in flight (`ps`)."""
        end = time.monotonic() + bound
        while time.monotonic() < end:
            tasks = hk.send("kernel", {"type": "ps"}).get("tasks") or []
            if not any(t.get("agent") == AI for t in tasks):
                return True
            time.sleep(0.05)
        return False

    script = [
        # 1 · a call: reflect the root
        (f'Looking around first.\n<call name="send">\n<target_id>{root}</target_id>\n'
         '<type>reflect</type>\n<tree>none</tree>\n</call>'),
        # 2 · a file write, bytes verbatim
        (f'<call name="send">\n<target_id>{FILES}</target_id>\n<type>write</type>\n'
         f'<path>out.txt</path>\n<bytes>{TOKEN}\n"quoted" & <raw></bytes>\n</call>'),
        # 3 · calls written wrong
        ('I will write it.\n<tool_call><function=call name="send"><target_id>files'
         '</target_id><type>write</type><path>bad.txt</path></function></tool_call>\n'
         f'<call name="send">oops<target_id>{FILES}</target_id><type>reflect</type></call>'),
        # 4 · a call inside the thought only
        (f'<think>I could run <call name="send"><target_id>{FILES}</target_id>'
         '<type>write</type><path>think.txt</path><bytes>x</bytes></call> but I will not.'
         '</think>Thinking it over.'),
        # 5 · a plain answer, and the chain ends
        'All finished: the file is written.\n<call name="done"></call>',
    ]
    try:
        r = hk.send(root, {"type": "update_agent", "id": "memory",
                           "callable_by": {**callable_by("memory"), AI: "*"}})
        need(r.get("updated") is True, f"memory accepts the AI: {str(r)[:200]}")
        rec = hk.create(parent_id="rest", handler_module="file_bridge.tools", id=FILES,
                        root="files", allow=["."], ingress_rule="allow_all_ingress",
                        callable_by={"$self": "*", "$parent": "*", AI: "*"})
        need(rec.get("id") == FILES, f"create {FILES}: {str(rec)[:200]}")
        rec = hk.create(parent_id="rest", handler_module="vllm_backend.tools", id=AI,
                        endpoint=m.url, model=MODEL, max_tokens=8192,
                        context_window=262144, kv_slots=4, kv_overflow="evict",
                        memory_store_id="memory",
                        can_call={"$self": "*", "memory": "*", root: ["reflect"],
                                  FILES: "*"},
                        callable_by={"$self": "*", "$parent": "*", "rest": "*"})
        need(rec.get("id") == AI, f"create {AI}: {str(rec)[:200]}")
        guid = hk.send(root, {"type": "reflect", "tree": "none"}).get("guid")

        m.reply(*script)
        out = hk.send(AI, {"type": "send", "text": f"Write {TOKEN} to out.txt."})
        need(not out.get("error"), f"the send is accepted: {str(out)[:200]}")
        turns = turns_at_least(len(script))
        need(idle(), "the chain settles (no AI task left in `ps`)")
        turns = m.turns()
        for i, t in enumerate(turns):
            print(f"  ── request {i + 1}: last user turn ──\n"
                  + "\n".join("     " + ln for ln in last_user(t["body"])[:400].splitlines()),
                  flush=True)
        need(len(turns) == len(script),
             f"one request per scripted answer, none after `done`: {len(turns)} for {len(script)}")
        u = [last_user(t["body"]) for t in turns] + [""] * len(script)

        # 1 → 2
        need("[reply to " in u[1] and f"{root} reflect" in u[1] and bool(guid) and guid in u[1],
             f"request 2 carries the reflect reply with the root guid {guid}")
        # 2 → 3
        f = work / "files" / "out.txt"
        body = f.read_bytes() if f.exists() else None
        need(body == f'{TOKEN}\n"quoted" & <raw>'.encode(),
             f"the write landed verbatim: {body!r}")
        need("[reply to " in u[2] and f"{FILES} write" in u[2],
             f"request 3 carries the write reply: {u[2][:160]!r}")
        # 3 → 4
        need(not (work / "files" / "bad.txt").exists(), "a call written wrong ran nothing")
        need("[malformed call — nothing ran] line 2, col 12" in u[3] and "^~" in u[3]
             and "write it as:" in u[3] and '<call name="send">' in u[3],
             f"request 4 carries the compiler-style report: {u[3][:200]!r}")
        need("call NOT dispatched: text outside a tag (`oops`)" in u[3],
             "request 4 carries the broken envelope's fault")
        # 4 → 5
        need(not (work / "files" / "think.txt").exists(),
             "a call inside the thought was not dispatched")
        hist = json.dumps((turns[4]["body"].get("messages") if len(turns) > 4 else None) or [])
        need("Thinking it over." in hist and "think.txt" not in hist,
             "the thought is stripped before the turn is stored: request 5 holds the "
             "answer, not the thought")
        need("[no call was dispatched]" in u[4],
             f"request 5 carries the no-call reminder: {u[4][:160]!r}")
        # across the run
        bodies = [t["body"] for t in turns]
        need(all(b.get("stream") is True and b.get("model") == MODEL and "tools" not in b
                 for b in bodies),
             "every request: streamed, the record's model, no `tools` array")
        probes = sorted({r["path"] for r in m.requests if r["kind"] != "turn"})
        need(not probes, f"nothing read from the server (every field set): {probes}")
    finally:
        hk.close()
        m.stop()
        shutil.rmtree(work, ignore_errors=True)

    verdict(not bad, f"{RUNTIME}: {len(bad)} failed" + (f" — first: {bad[0]}" if bad else ""))
    return 0 if not bad else 1


if __name__ == "__main__":
    sys.exit(main())
