"""kernel/autofold — autofold (#556) against a SCRIPTED model. NO MODEL.

A local OpenAI-compatible server plays the model; the kernel's own AI agent (on both runtimes: an
`ai_chat.tools` chat naming a `vllm_backend` provider) on
the PACKAGED host of `RUNTIME` (python | rust) runs the chains, driven through `web_rest`,
next to a `file_bridge` agent (`files`) holding an 80 kB file.

  P1. the AI reads the file (thought, then a `read` call); the reply is whole in the
      next request; the AI answers `done`;
  P2. the next send: the read reply is its `[folded · read reply …]` anchor and both
      thoughts are `[thought ~N tokens …]` anchors (none of their text); the AI then
      recalls row B by id and gets the body whole;
  P3. after the next answer the recall reply is `[folded · recall reply … id B …]`; the
      store row B is still whole;
  P4. a cut answer after thinking is its `[cut · …]` anchor in the correction turn.
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
AI, FILES, PROV = "ai", "files", "ai_prov"
MODEL = "mock-scripted"
TOKEN = "E141-" + secrets.token_hex(4)
DONE = '<call name="done"></call>'
CUT = ('<think>CUT-' + TOKEN + '</think>Sending it.\n<invoke name="send">\n'
       + '</invoke>\n' * 50)


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




def messages_of(turn: dict) -> list[dict]:
    return turn["body"].get("messages") or []


def blob(turn: dict) -> str:
    return json.dumps(messages_of(turn), ensure_ascii=False)


def user_texts(turn: dict) -> list[str]:
    return [str(m.get("content")) for m in messages_of(turn) if m.get("role") == "user"]


def main() -> int:
    name, _ = runtime(REPO, RUNTIME)
    spec = ensure_runtime(REPO, name)
    m = ThinkingVllm(model=MODEL).start()
    work = Path(tempfile.mkdtemp(prefix="e2e-autofold-"))
    (work / "files").mkdir()
    # 100 lines of 800 characters: the `read` reply (default 200 lines) is over 64,000.
    (work / "files" / "big.txt").write_text(
        "".join(f"L{n:03d} " + "x" * 795 + "\n" for n in range(1, 101)))
    hk = HostKernel(spec, work, reach=[AI, FILES, PROV]).boot()
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

    def turns_at_least(n: int, bound: float = 90) -> list[dict]:
        end = time.monotonic() + bound
        while time.monotonic() < end:
            got = m.turns()
            if len(got) >= n:
                return got
            time.sleep(0.05)
        return m.turns()

    def idle(bound: float = 90) -> bool:
        end = time.monotonic() + bound
        while time.monotonic() < end:
            tasks = hk.send("kernel", {"type": "ps"}).get("tasks") or []
            if not any(t.get("agent") == AI for t in tasks):
                return True
            time.sleep(0.05)
        return False

    def history() -> list[dict]:
        h = hk.send(AI, {"type": "history", "limit": 500})
        return h.get("rows") or h.get("history") or h.get("messages") or []

    def run(label: str, text: str, want: int) -> list[dict]:
        out = hk.send(AI, {"type": "send", "text": text})
        need(not out.get("error"), f"{label}: the send is accepted: {str(out)[:200]}")
        turns_at_least(want)
        need(idle(), f"{label}: the chain settles")
        time.sleep(0.5)
        turns = m.turns()
        need(len(turns) == want, f"{label}: {want} requests: {len(turns)}")
        return turns

    read = (f'<call name="send"><routing><target_id>{FILES}</target_id></routing>'
            '<payload><type>read</type><path>big.txt</path></payload></call>')
    try:
        r = hk.send(root, {"type": "update_agent", "id": "memory",
                           "callable_by": {**callable_by("memory"), AI: "*", PROV: "*"}})
        need(r.get("updated") is True, f"memory accepts the AI: {str(r)[:200]}")
        rec = hk.create(parent_id="rest", handler_module="file_bridge.tools", id=FILES,
                        root="files", allow=["."], ingress_rule="allow_all_ingress",
                        callable_by={"$self": "*", "$parent": "*", AI: "*"})
        need(rec.get("id") == FILES, f"create {FILES}: {str(rec)[:200]}")
        can = {"$self": "*", "memory": "*", FILES: "*"}
        by = {"$self": "*", "$parent": "*", "rest": "*"}
        # a provider (the vllm_backend) and the chat (ai_chat.tools) naming it:
        # a `send` to a provider is refused (#521)
        rec = hk.create(parent_id="rest", handler_module="vllm_backend.tools", id=PROV,
                        endpoint=m.url, model=MODEL, max_tokens=8192,
                        context_window=262144, kv_slots=4, kv_overflow="evict",
                        memory_store_id="memory", can_call={"$self": "*", "memory": "*"},
                        callable_by={"$self": "*", "$parent": "*", AI: "*"})
        need(rec.get("id") == PROV, f"create {PROV}: {str(rec)[:200]}")
        rec = hk.create(parent_id="rest", handler_module="ai_chat.tools", id=AI,
                        provider_id=PROV, model=MODEL, max_tokens=8192,
                        context_window=262144, memory_store_id="memory",
                        can_call={**can, PROV: ["complete", "watch", "reflect"]}, callable_by=by)
        need(rec.get("id") == AI, f"create {AI}: {str(rec)[:200]}")

        # ── P1 · the read reply is whole the turn it arrives ──────────────────
        m.reply(f"<think>PLAN-{TOKEN}</think>Reading.\n{read}",
                f'<think>SEEN-{TOKEN}</think>Got it.\n{DONE}')
        turns = run("P1", "read big.txt", 2)
        need(len(turns) > 1 and "L100 " in user_texts(turns[1])[-1],
             "P1: request 2 carries the whole read reply")
        big = [r for r in history() if r.get("role") == "user"
               and int(r.get("bytes") or 0) > 64000]
        need(len(big) == 1, f"P1: the store holds the read reply whole: "
             f"{[r.get('bytes') for r in history()]}")
        if not big:
            raise SystemExit(verdict(False, f"{RUNTIME}: no large row to fold"))
        b = big[0]["id"]

        # ── P2 · folded in the next request; recall by id gives it back ───────
        m.clear()
        m.reply(f'<call name="send"><routing><target_id>{AI}</target_id></routing>'
                f'<payload><type>recall</type><id>{b}</id></payload></call>',
                f'Noted.\n{DONE}')
        turns = run("P2", "next", 2)
        seen = blob(turns[0]) if turns else ""
        need("[folded · read reply from files · ~" in seen and f"get: {b}]" in seen,
             "P2: request 1 shows the read reply as its anchor")
        need("[thought ~" in seen, "P2: request 1 shows the thoughts as anchors")
        need(f"PLAN-{TOKEN}" not in seen and f"SEEN-{TOKEN}" not in seen and "L100 " not in seen,
             "P2: request 1 holds no thought text and no body")
        last = user_texts(turns[1])[-1] if len(turns) > 1 else ""
        need(" ai recall" in last and "L100 " in last,
             "P2: request 2 carries the recalled row whole")

        # ── P3 · the recall reply folds after the next answer ─────────────────
        m.clear()
        m.reply(DONE)
        turns = run("P3", "again", 1)
        seen = blob(turns[0]) if turns else ""
        need(f"[folded · recall reply from ai · id {b} · ~" in seen,
             "P3: the recall reply is its anchor")
        need("L100 " not in seen, "P3: the body is nowhere in the request")
        need(any(r.get("id") == b and int(r.get("bytes") or 0) > 64000 for r in history()),
             "P3: the store row is still whole")

        # ── P4 · a cut answer is its anchor in the correction turn ────────────
        m.clear()
        m.reply(CUT, DONE)
        turns = run("P4", "cut it", 2)
        cut = [str(x.get("content")) for x in (messages_of(turns[1]) if len(turns) > 1 else [])
               if x.get("role") == "assistant"]
        need(any('[cut · you wrote <invoke name="send"> after thinking ~' in c
                 and "ACTION: " in c and "full: " in c for c in cut),
             f"P4: request 2 shows the cut answer as its anchor: {cut[-1:]!r}")
        need(f"CUT-{TOKEN}" not in (blob(turns[1]) if len(turns) > 1 else ""),
             "P4: the cut thought is not in the request")
    finally:
        hk.close()
        m.stop()
        shutil.rmtree(work, ignore_errors=True)

    verdict(not bad, f"{RUNTIME}: {len(bad)} failed" + (f" — first: {bad[0]}" if bad else ""))
    return 0 if not bad else 1


if __name__ == "__main__":
    sys.exit(main())
