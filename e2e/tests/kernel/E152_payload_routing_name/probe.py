"""kernel/payload_routing_name — a routing name inside `<payload>` reaches its target. NO MODEL.

A local OpenAI-compatible server (`e2e/mocks/vllm.py`) plays the model; the kernel's own
`ai_chat.tools` chat completes through a `vllm_backend` provider on the PACKAGED Rust host,
every call from the test through `web_rest`. The target is a test-only folder bundle
`notekeep` (written at runtime, as E117 and E149) whose verb `create` documents
`title` and `body`; it appends the payload it was given to `calls.jsonl` in the work folder
and answers `{created, title, body}`.

SCRIPT, one model answer per request:
  1. the routing form: `<routing>` names the target and a routing `<title>` that labels the
     call; `<payload>` carries `<type>create</type>`, its own `<title>` and `<body>`. The
     payload is delivered WHOLE: the target received the payload's title, not the label,
     and the next request carries the reply with the payload's title;
  2. the same call in the INLINE form (`<target_id>`, `<title>` beside `<payload>`): `title`
     there is call routing and the verb documents it, so the call is refused, loud, before it
     runs: the next request carries the refusal and `calls.jsonl` has no second line;
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

import yaml
from helpers.host.driver import HostKernel, ensure_runtime, runtime
from helpers.verdict import verdict
from mocks.vllm import VllmMock

REPO = E2E.parent
RUNTIME = os.environ.get("RUNTIME", "")
HELPER = (REPO / "src/lib/python/bundled_agents/io/py_agent/src/py_agent"
          / "fantastic_agent.py")
AI, PROV, KB = "ai", "ai_prov", "kb"
BUNDLE = "notekeep"
MODEL = "mock-scripted"
TOKEN = secrets.token_hex(4)
TITLE = f"PAYLOAD-TITLE-{TOKEN}"
LABEL = f"make-notes-{TOKEN}"
BODY = f"BODY-{TOKEN}"
DONE = '<call name="done"></call>'


def write_bundle(work: Path, log: Path) -> None:
    folder = work / ".fantastic" / "bundles" / BUNDLE
    folder.mkdir(parents=True, exist_ok=True)
    shutil.copy(HELPER, folder / "fantastic_agent.py")
    (folder / "agent.py").write_text(
        "import json\nimport fantastic_agent as fa\n\n"
        "def create(p):\n"
        f"    with open({str(log)!r}, 'a') as f:\n"
        "        f.write(json.dumps(p, sort_keys=True) + '\\n')\n"
        "    return {'created': True, 'title': p.get('title'), 'body': p.get('body')}\n\n"
        "fa.serve({'create': create})\n")
    (folder / "bundle.yaml").write_text(yaml.safe_dump({
        "base": "py_agent",
        "sentence": "The note-keeping test agent.",
        "command": [sys.executable, "agent.py"],
        "verbs": {"create": "args: title:str (req), body:str (req). Files a note → {created, title, body}."},
    }, sort_keys=False))


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
    log = work / "calls.jsonl"
    write_bundle(work, log)
    m = VllmMock(model=MODEL).start()
    hk = HostKernel(spec, work, reach=[AI, PROV, KB]).boot()
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

    def calls() -> list[dict]:
        return [json.loads(ln) for ln in log.read_text().splitlines()] if log.exists() else []

    routed = (f'<call name="send"><routing><target_id>{KB}</target_id><title>{LABEL}</title></routing>'
              f"<payload><type>create</type><title>{TITLE}</title><body>{BODY}</body></payload></call>")
    inline = (f'<call name="send"><target_id>{KB}</target_id><title>{LABEL}</title>'
              f"<payload><type>create</type><body>{BODY}</body></payload></call>")
    script = [routed, inline, f"Saw both. {DONE}"]
    try:
        r = hk.send(hk.root_id, {"type": "update_agent", "id": "memory",
                                 "callable_by": {**callable_by("memory"), AI: "*", PROV: "*"}})
        need(r.get("updated") is True, f"memory accepts the AI: {str(r)[:200]}")
        rec = hk.create(parent_id="rest", handler_module=BUNDLE, id=KB,
                        callable_by={"$self": "*", "$parent": "*", AI: "*"})
        need(rec.get("id") == KB, f"create {KB} from the folder bundle: {str(rec)[:200]}")
        rec = hk.create(parent_id="rest", handler_module="vllm_backend.tools", id=PROV,
                        endpoint=m.url, model=MODEL, max_tokens=8192,
                        context_window=262144, kv_slots=4, kv_overflow="evict",
                        memory_store_id="memory", can_call={"$self": "*", "memory": "*"},
                        callable_by={"$self": "*", "$parent": "*", AI: "*"})
        need(rec.get("id") == PROV, f"create {PROV}: {str(rec)[:200]}")
        rec = hk.create(parent_id="rest", handler_module="ai_chat.tools", id=AI,
                        provider_id=PROV, model=MODEL, max_tokens=8192,
                        context_window=262144, memory_store_id="memory",
                        can_call={"$self": "*", "memory": "*", KB: "*", PROV: ["complete", "watch"]},
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
        got = calls()
        need(len(got) >= 1 and got[0].get("title") == TITLE and got[0].get("body") == BODY
             and got[0].get("type") == "create",
             f"the target received the payload whole, its own title and body: {got[:1]}")
        need(LABEL not in json.dumps(got[:1]), "the routing title labels the call, it was not delivered")
        need(TITLE in u[1] and "created" in u[1],
             f"request 2 carries the target's reply with the payload's title: {u[1][:300]!r}")
        # 2 · the inline form: refused loud, the verb does not run
        need("title on this call is call routing" in u[2] and "documents an argument named title" in u[2],
             f"request 3 carries the refusal: {u[2][:300]!r}")
        need(len(calls()) == 1, f"the refused call did not run: {len(calls())} runs of create")
    finally:
        hk.close()
        m.stop()
        shutil.rmtree(work, ignore_errors=True)

    verdict(not bad, f"{RUNTIME}: {len(bad)} failed" + (f" — first: {bad[0]}" if bad else ""))
    return 0 if not bad else 1


if __name__ == "__main__":
    sys.exit(main())
