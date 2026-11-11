"""kernel/payload_routing_name — a routing name inside `<payload>` reaches its target. NO MODEL.

A local OpenAI-compatible server (`e2e/mocks/vllm.py`) plays the model; the kernel's own
`ai_chat.tools` chat completes through a `vllm_backend` provider on the PACKAGED Rust host,
every call from the test through `web_rest`. The target is the kernel's own `create_agent`
(`**meta`: every field the payload carries becomes a field of the new record), so `title`
is visible on the record it makes: no fixture, nothing written by the test but the script.

SCRIPT, one model answer per request:
  1. the routing form: `<routing>` names the target and a routing `<title>` that labels the
     call; `<payload>` carries `<type>create_agent</type>` and its own `<title>`. The payload
     is delivered WHOLE: the new agent `made` holds the payload's title, not the label;
  2. the same call in the INLINE form (`<target_id>`, `<title>` beside the verb's own arguments, no `<payload>`): there
     `title` is call routing, lifted out and never sent: the new agent `made2` holds no
     title (the verb does not document `title`, so no refusal; a verb that does is refused
     loud, which the `fantastic-ai-core` unit
     `a_routing_name_in_the_payload_of_a_routed_call_is_delivered` holds);
  3. a plain answer + `done`.
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

from helpers.host.driver import HostKernel, ensure_runtime, runtime
from helpers.verdict import verdict
from mocks.vllm import VllmMock

REPO = E2E.parent
RUNTIME = os.environ.get("RUNTIME", "")
AI, PROV = "ai", "ai_prov"
MADE, MADE2 = "made", "made2"
MODEL = "mock-scripted"
TOKEN = secrets.token_hex(4)
TITLE = f"PAYLOAD-TITLE-{TOKEN}"
LABEL = f"make-notes-{TOKEN}"
DONE = '<call name="done"></call>'


def last_user(turn: dict) -> str:
    for m in reversed(turn["body"].get("messages") or []):
        if m.get("role") == "user":
            c = m.get("content")
            return c if isinstance(c, str) else json.dumps(c)
    return ""


def main() -> int:
    name, _ = runtime(REPO, RUNTIME)
    spec = ensure_runtime(REPO, name)
    work = Path(tempfile.mkdtemp(prefix="e2e-payload-routing-"))
    m = VllmMock(model=MODEL).start()
    hk = HostKernel(spec, work, reach=[AI, PROV]).boot()
    bad: list[str] = []

    def need(cond: bool, what: str) -> None:
        print(f"  {'ok ' if cond else 'BAD'} {what}", flush=True)
        if not cond:
            bad.append(what)

    def turns_at_least(n: int, bound: float = 60) -> list[dict]:
        end = time.monotonic() + bound
        while time.monotonic() < end:
            got = m.turns()
            if len(got) >= n:
                return got
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

    def callable_by(aid: str) -> dict:
        r = hk.send("kernel", {"type": "list_agents"})
        rec = next(a for a in r.get("agents") or [] if a.get("id") == aid)
        return dict(rec.get("callable_by") or {})

    def record(aid: str) -> dict:
        r = hk.send("kernel", {"type": "list_agents"})
        return next((a for a in r.get("agents") or [] if a.get("id") == aid), {})

    def made(aid: str, extra: str) -> str:
        return (f"<type>create_agent</type><handler_module>ram_store.tools</handler_module>"
                f"<parent_id>rest</parent_id><id>{aid}</id>{extra}")

    routed = (f'<call name="send"><routing><target_id>kernel</target_id><title>{LABEL}</title></routing>'
              f"<payload>{made(MADE, f'<title>{TITLE}</title>')}</payload></call>")
    inline = (f'<call name="send"><target_id>kernel</target_id><title>{LABEL}</title>'
              f"{made(MADE2, '')}</call>")
    script = [routed, inline, f"Saw both. {DONE}"]
    try:
        r = hk.send(hk.root_id, {"type": "update_agent", "id": "memory",
                                 "callable_by": {**callable_by("memory"), AI: "*", PROV: "*"}})
        need(r.get("updated") is True, f"memory accepts the AI: {str(r)[:200]}")
        rec = hk.create(parent_id="rest", handler_module="vllm_backend.tools", id=PROV,
                        endpoint=m.url, model=MODEL, max_tokens=8192,
                        context_window=262144, kv_slots=4, kv_overflow="evict",
                        memory_store_id="memory", can_call={"$self": "*", "memory": "*"},
                        callable_by={"$self": "*", "$parent": "*", AI: "*"})
        need(rec.get("id") == PROV, f"create {PROV}: {str(rec)[:200]}")
        rec = hk.create(parent_id="rest", handler_module="ai_chat.tools", id=AI,
                        provider_id=PROV, model=MODEL, max_tokens=8192,
                        context_window=262144, memory_store_id="memory",
                        can_call={"$self": "*", "memory": "*", **{hk.root_id: ["create_agent"]}, PROV: ["complete", "watch"]},
                        callable_by={"$self": "*", "$parent": "*", "rest": "*"})
        need(rec.get("id") == AI, f"create {AI}: {str(rec)[:200]}")

        m.reply(*script)
        out = hk.send(AI, {"type": "send", "text": f"Make notes ({TOKEN})."})
        need(not out.get("error"), f"the send is accepted: {str(out)[:200]}")
        turns_at_least(len(script))
        need(idle(), "the chain settles (no AI task left in `ps`)")
        turns = m.turns()
        for i, t in enumerate(turns):
            print(f"  ── request {i + 1}: last user turn ──\n"
                  + "\n".join("     " + ln for ln in last_user(t)[:500].splitlines()), flush=True)
        need(len(turns) == len(script), f"one request per scripted answer: {len(turns)} for {len(script)}")
        u = [last_user(t) for t in turns] + [""] * len(script)

        # 1 · the routing form: the payload's title reaches the target
        rec = record(MADE)
        need(rec.get("title") == TITLE,
             f"the new agent holds the payload's own title: {rec.get('title')!r}")
        need(LABEL not in json.dumps(rec), "the routing title labels the call, it was not delivered")
        need(MADE in u[1] and "error" not in u[1].lower(),
             f"request 2 carries the create reply: {u[1][:300]!r}")
        # 2 · the inline form: `title` is routing there, lifted out, never sent
        rec2 = record(MADE2)
        need(bool(rec2) and "title" not in rec2 and LABEL not in json.dumps(rec2),
             f"the inline call made {MADE2} without a title: {rec2}")
    finally:
        hk.close()
        m.stop()
        shutil.rmtree(work, ignore_errors=True)

    verdict(not bad, f"{RUNTIME}: {len(bad)} failed" + (f" — first: {bad[0]}" if bad else ""))
    return 0 if not bad else 1


if __name__ == "__main__":
    sys.exit(main())
