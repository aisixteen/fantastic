"""kernel/scripted_ai — an AI agent's call loop against a SCRIPTED model. NO MODEL.

A local OpenAI-compatible server (`e2e/mocks/vllm.py`, extended here with vLLM's
thought channel `reasoning_content`) plays the model; the kernel's own
`ai_chat.tools` chat (ai_core), completing through a `vllm_backend` provider, on the
PACKAGED rust host runs the chain, every call from the test through `web_rest`. Tool calling
is the prompt-taught `<call name>` tag grammar — the request carries no `tools`.
Every field the backend needs is SET (endpoint, model, context_window, kv_slots,
kv_overflow on the provider; max_tokens on the chat, #632), so nothing is read from
the server.

The script, one model answer per request:

  1. `<call name="send">` → `reflect` on the root: dispatched; its result comes
     back as the NEXT request's last user turn, `[reply to <pid> · kernel_state
     reflect]` carrying the root's id and NOT its guid (the root's runtime frame
     is the operator's; the AI holds `call: [reflect]` on the root);
  2. a `write` to a file_bridge, `bytes` verbatim: the file lands on disk with
     exactly those bytes; the next request carries the `write` reply;
  3-5 are ONE turn, re-asked inside it (#766: at most `1 + CALL_RETRIES` = 3
  attempts, `constants.rs` `CALL_RETRIES = 2`):
  3. attempt 1, a call written WRONG — a `<tool_call><function=call name="send">`
     wrapper: the attempt fails at the first foreign tag, nothing after it is read;
     nothing runs (no file). Request 4 is the RE-ASK: request 3's messages unchanged,
     then the attempt as written (cut there) and the compiler-style report
     (`[malformed call — nothing ran] line L, col C`, a caret, `write it as:` + the
     form) as the last user message;
  4. attempt 2, an envelope with text outside a tag: not dispatched; request 5 is
     request 4's messages, then this attempt and its fault (`call NOT dispatched:
     text outside a tag`);
  5. attempt 3, a call INSIDE the thought (`reasoning_content`), an answer with
     none: accepted (a call-less answer is not a malformed one); nothing is
     dispatched (no file); request 6 carries the no-call reminder (`[no call was
     dispatched]`), its history holds the answer without the thought (stripped
     before the turn is stored, shown as its `[thought ~N tokens …]` anchor) and
     NOTHING of attempts 1 and 2: neither attempt nor report is stored or re-sent;
  6. a plain answer + `done`: the chain ends — no further request reaches the
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
from pathlib import Path

E2E = next(d for d in Path(__file__).resolve().parents if (d / ".e2e-root").exists())
sys.path.insert(0, str(E2E))

import yaml

from helpers.host.driver import HostKernel, ensure_runtime, runtime
from helpers.lines import ALL, PROVIDER, ROOT, call
from helpers.verdict import verdict
from mocks.vllm import ThinkingVllmMock

REPO = E2E.parent
RUNTIME = os.environ.get("RUNTIME", "")
AI, FILES, PROV = "ai", "files", "ai_prov"
MODEL = "mock-scripted"
TOKEN = "E119-" + secrets.token_hex(4)


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
    m = ThinkingVllmMock(model=MODEL).start()
    work = Path(tempfile.mkdtemp(prefix="e2e-scripted-ai-"))
    (work / "files").mkdir()
    hk = HostKernel(spec, work).boot()
    root = hk.root_id
    bad: list[str] = []

    def need(cond: bool, what: str) -> None:
        print(f"  {'ok ' if cond else 'BAD'} {what}", flush=True)
        if not cond:
            bad.append(what)

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
        (f'Looking around first.\n<call name="send">\n<routing><target_id>{root}</target_id></routing>\n'
         '<payload><type>reflect</type>\n<tree>none</tree></payload>\n</call>'),
        # 2 · a file write, bytes verbatim
        (f'<call name="send">\n<routing><target_id>{FILES}</target_id></routing>\n<payload><type>write</type>\n'
         f'<path>out.txt</path>\n<bytes>{TOKEN}\n"quoted" & <raw></bytes></payload>\n</call>'),
        # 3 · attempt 1 of one turn: a call written wrong, failed at the first foreign tag
        ('I will write it.\n<tool_call><function=call name="send"><target_id>files'
         '</target_id><type>write</type><path>bad.txt</path></function></tool_call>'),
        # 4 · attempt 2: an envelope with text outside a tag
        f'<call name="send">oops<routing><target_id>{FILES}</target_id></routing><payload><type>reflect</type></payload></call>',
        # 5 · attempt 3: a call inside the thought only (accepted: no call)
        (f'<think>I could run <call name="send"><routing><target_id>{FILES}</target_id></routing><payload>'
         '<type>write</type><path>think.txt</path><bytes>x</bytes></payload></call> but I will not.'
         '</think>Thinking it over.'),
        # 6 · a plain answer, and the chain ends
        'All finished: the file is written.\n<call name="done"></call>',
    ]
    req2 = ""

    def history() -> list[dict]:
        h = hk.send(AI, {"type": "history", "limit": 500})
        return h.get("rows") or h.get("history") or h.get("messages") or []

    try:
        rec = hk.create(parent_id="rest", handler_module="file_bridge.tools", id=FILES,
                        root="files", allow=["."], ingress_rule="allow_all_ingress")
        need(rec.get("id") == FILES, f"create {FILES}: {str(rec)[:200]}")
        # the AI's own lines; the root line is capped by `rest` and `web`, which hold it
        can = {"memory": ALL, ROOT: call("reflect"), FILES: ALL}
        # a provider (the vllm_backend) and the chat (ai_chat.tools) naming it:
        # a `send` to a provider is refused (#521)
        rec = hk.create(parent_id="rest", handler_module="vllm_backend.tools", id=PROV,
                        endpoint=m.url, model=MODEL,
                        context_window=262144, kv_slots=4, kv_overflow="evict",
                        memory_store_id="memory", can={"memory": ALL})
        need(rec.get("id") == PROV, f"create {PROV}: {str(rec)[:200]}")
        rec = hk.create(parent_id="rest", handler_module="ai_chat.tools", id=AI,
                        provider_id=PROV, model=MODEL, max_tokens=8192,
                        context_window=262144, memory_store_id="memory",
                        can={**can, PROV: PROVIDER})
        need(rec.get("id") == AI, f"create {AI}: {str(rec)[:200]}")

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

        # 1 → 2 (the guid half is checked once the kernel is down, from its state folder)
        req2 = u[1]
        need("[reply to " in u[1] and f"{root} reflect" in u[1] and f"<id>{root}</id>" in u[1],
             f"request 2 carries the root's reflect reply (its id): {u[1][:200]!r}")
        # 2 → 3
        f = work / "files" / "out.txt"
        body = f.read_bytes() if f.exists() else None
        need(body == f'{TOKEN}\n"quoted" & <raw>'.encode(),
             f"the write landed verbatim: {body!r}")
        need("[reply to " in u[2] and f"{FILES} write" in u[2],
             f"request 3 carries the write reply: {u[2][:160]!r}")
        # 3 → 4 → 5: one turn, re-asked inside it (#766)
        msgs = [[(x.get("role"), x.get("content")) for x in (t["body"].get("messages") or [])]
                for t in turns] + [[]] * len(script)
        need(not (work / "files" / "bad.txt").exists(), "a call written wrong ran nothing")
        need("[malformed call — nothing ran] line 2, col 12" in u[3] and "^~" in u[3]
             and "write it as:" in u[3] and '<call name="send">' in u[3],
             f"request 4 carries the compiler-style report: {u[3][:200]!r}")
        need(len(msgs[3]) == len(msgs[2]) + 2 and msgs[3][:-2] == msgs[2],
             "request 4 is the re-ask: request 3's messages unchanged, then one pair")
        reask1 = msgs[3][-2] if len(msgs[3]) >= 2 else (None, "")
        need(reask1[0] == "assistant" and str(reask1[1]).startswith("I will write it.\n<tool_call>"),
             f"request 4's re-ask pair holds attempt 1 as written: {str(reask1[1])[:120]!r}")
        need("bad.txt" not in json.dumps(msgs[3]),
             "attempt 1 is read no further than its first foreign tag: nothing after it is quoted")
        need("call NOT dispatched: text outside a tag (`oops`)" in u[4],
             "request 5 carries the broken envelope's fault")
        need(len(msgs[4]) == len(msgs[3]) + 2 and msgs[4][:-2] == msgs[3]
             and msgs[4][-2][0] == "assistant" and "oops" in str(msgs[4][-2][1]),
             "request 5 is the second re-ask: request 4's messages, then attempt 2 and its fault")
        # 5 → 6
        need(not (work / "files" / "think.txt").exists(),
             "a call inside the thought was not dispatched")
        gone = ("I will write it.", "<tool_call>", "oops", "[malformed call", "text outside a tag")
        later = json.dumps([[x for x in r if x[0] != "system"] for r in msgs[5:len(turns)]])
        need(not any(g in later for g in gone),
             f"no request after the accepted attempt holds attempt 1, attempt 2 or a report: "
             f"{[g for g in gone if g in later]}")
        stored = json.dumps(history())
        need(not any(g in stored for g in gone),
             f"the store holds neither failed attempt nor a report: "
             f"{[g for g in gone if g in stored]}")
        hist = json.dumps((turns[5]["body"].get("messages") if len(turns) > 5 else None) or [])
        need("Thinking it over." in hist and "think.txt" not in hist,
             "the thought is stripped before the turn is stored: request 6 holds the "
             "answer, not the thought")
        need(any("[thought ~" in m.get("content", "")
                 for m in turns[5]["body"]["messages"] if m.get("role") == "assistant"),
             "request 6 shows the thought as its anchor")
        need("[no call was dispatched]" in u[5],
             f"request 6 carries the no-call reminder: {u[5][:160]!r}")
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
        # the root's guid, from its state folder (root fields are operator only)
        root_rec = yaml.safe_load((work / ".fantastic" / "agent.yaml").read_text()) or {}
        guid = root_rec.get("guid")
        shutil.rmtree(work, ignore_errors=True)
    need(isinstance(guid, str) and bool(guid) and guid not in req2,
         f"request 2 carries no part of the root's guid {guid!r} (operator only)")

    verdict(not bad, f"{RUNTIME}: {len(bad)} failed" + (f" — first: {bad[0]}" if bad else ""))
    return 0 if not bad else 1


if __name__ == "__main__":
    sys.exit(main())
