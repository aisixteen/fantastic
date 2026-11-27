"""kernel/payload_routing_name — a routing name inside `<payload>` reaches its target. NO MODEL.

A local OpenAI-compatible server (`e2e/mocks/vllm.py`) plays the model; the kernel's own
`ai_chat.tools` chat completes through a `vllm_backend` provider on the PACKAGED Rust host,
every call from the test through `web_rest`. The target is the kernel's own `create_agent`, which takes `id` (the new agent's id), and `id`
is also a routing name (the call's label): the one name that shows both readings.

SCRIPT, one model answer per request:
  1. the routing form: `<routing>` names the target and a routing `<id>` that labels the
     call; `<payload>` carries `<type>create_agent</type>` and its own `<id>`. The payload
     is delivered WHOLE: the new agent `made` exists under the payload's id and the label
     appears nowhere in its record;
  2. the same call in the INLINE form (`<target_id>` and the verb's arguments directly in the
     call, no `<routing>`, no `<payload>`): the parser refuses it (one call form, #528), the
     refusal shows the routing form, and `made2` is never created;
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
from helpers.lines import ALL, PROVIDER, create, is_unknown
from helpers.verdict import verdict
from mocks.vllm import VllmMock

REPO = E2E.parent
RUNTIME = os.environ.get("RUNTIME", "")
AI, PROV = "ai", "ai_prov"
MADE, MADE2 = "made", "made2"
MODEL = "mock-scripted"
TOKEN = secrets.token_hex(4)
LABEL = f"make-notes-{TOKEN}"
DONE = '<call name="done"></call>'


def last_user(one_pass: dict) -> str:
    for m in reversed(one_pass["body"].get("messages") or []):
        if m.get("role") == "user":
            c = m.get("content")
            return c if isinstance(c, str) else json.dumps(c)
    return ""


def main() -> int:
    name, _ = runtime(REPO, RUNTIME)
    spec = ensure_runtime(REPO, name)
    work = Path(tempfile.mkdtemp(prefix="e2e-payload-routing-"))
    m = VllmMock(model=MODEL).start()
    hk = HostKernel(spec, work).boot()
    bad: list[str] = []

    def need(cond: bool, what: str) -> None:
        print(f"  {'ok ' if cond else 'BAD'} {what}", flush=True)
        if not cond:
            bad.append(what)

    def passes_at_least(n: int, bound: float = 60) -> list[dict]:
        end = time.monotonic() + bound
        while time.monotonic() < end:
            got = m.passes()
            if len(got) >= n:
                return got
            time.sleep(0.05)
        return m.passes()

    def idle(bound: float = 60) -> bool:
        end = time.monotonic() + bound
        while time.monotonic() < end:
            if hk.quiet(AI):
                return True
            time.sleep(0.05)
        return False

    def record(aid: str) -> dict:
        """`aid`'s record (reflect); `{}` on the unknown answer (a missing id)."""
        r = hk.send(aid, {"type": "reflect"})
        return {} if not isinstance(r, dict) or is_unknown(r, aid) else r

    def made(aid: str, extra: str) -> str:
        return (f"<type>create_agent</type><handler_module>ram_store.tools</handler_module>"
                f"<parent_id>rest</parent_id><id>{aid}</id>{extra}")

    routed = (f'<call name="send"><routing><target_id>kernel</target_id><id>{LABEL}</id></routing>'
              f"<payload>{made(MADE, '')}</payload></call>")
    inline = (f'<call name="send"><target_id>kernel</target_id>'
              f"{made(MADE2, '')}</call>")
    script = [routed, inline, f"Saw both. {DONE}"]
    try:
        rec = hk.create(parent_id="rest", handler_module="vllm_backend.tools", id=PROV,
                        endpoint=m.url, model=MODEL,
                        context_window=262144, kv_slots=4, kv_overflow="evict",
                        memory_store_id="memory", can={"memory": ALL})
        need(rec.get("id") == PROV, f"create {PROV}: {str(rec)[:200]}")
        rec = hk.create(parent_id="rest", handler_module="ai_chat.tools", id=AI,
                        provider_id=PROV, model=MODEL, max_tokens=8192,
                        context_window=262144, memory_store_id="memory",
                        # the AI may create exactly these two ids under `rest`, as ram stores;
                        # `rest` hands the line out from its own `create *` on itself
                        can={"rest": create((MADE, MADE2), {"ram_store.tools": {"*": "*"}}),
                             "memory": ALL, PROV: PROVIDER})
        need(rec.get("id") == AI, f"create {AI}: {str(rec)[:200]}")

        m.reply(*script)
        out = hk.send(AI, {"type": "send", "text": f"Make notes ({TOKEN})."})
        need(not out.get("error"), f"the send is accepted: {str(out)[:200]}")
        passes_at_least(len(script))
        need(idle(), "the chain settles (no AI task left in `ps`)")
        passes = m.passes()
        for i, t in enumerate(passes):
            print(f"  ── request {i + 1}: last user message ──\n"
                  + "\n".join("     " + ln for ln in last_user(t)[:500].splitlines()), flush=True)
        need(len(passes) == len(script), f"one request per scripted answer: {len(passes)} for {len(script)}")
        u = [last_user(t) for t in passes] + [""] * len(script)

        # 1 · the routing form: the payload is delivered whole, its own `id` makes the agent
        rec = record(MADE)
        need(rec.get("id") == MADE and rec.get("handler_module") == "ram_store.tools",
             f"the new agent exists under the payload's own id: {rec}")
        need(LABEL not in json.dumps(rec), "the routing id labels the call, it was not delivered")
        need(MADE in u[1] and "error" not in u[1].lower(),
             f"request 2 carries the create reply: {u[1][:300]!r}")
        # 2 · the inline form: refused by the parser with the routing form, nothing made
        need("call NOT dispatched: `<target_id>` directly in the call" in u[2]
             and "<routing><target_id>agent id</target_id></routing>" in u[2]
             and not record(MADE2),
             f"the inline call is refused with the routing form and creates nothing: "
             f"{u[2][:300]!r} {record(MADE2)}")
    finally:
        hk.close()
        m.stop()
        shutil.rmtree(work, ignore_errors=True)

    verdict(not bad, f"{RUNTIME}: {len(bad)} failed" + (f" — first: {bad[0]}" if bad else ""))
    return 0 if not bad else 1


if __name__ == "__main__":
    sys.exit(main())
