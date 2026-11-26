"""agentic_advanced/subai_key_twin — the key line a builder hands its new chat. NO MODEL.

The free twin of the key wiring in `subai_graph` (E31): the builder `ai` holds the
key store line (`creds: get`, written by the operator beside its `create` line, as
E31's `_wire_builder` writes it) and hands the chat `hour_ai` its own in the
`create_agent` that makes it, before `hour_ai_prov` exists under the chat. Every
ancestor of the provider whose tree does not hold the key store caps its `creds`
call, so the chat's line is what lets the key through. The operator writes no line
on a model-made agent: in a GPU run of E31 an operator line written by a 2 s poll
after the chat existed was replaced by the builder's own `can` update, and every
completion of the provider failed `credentials creds/vllm: no agent 'creds'`.

Two local OpenAI-compatible servers (`e2e/mocks/vllm.py`) play the models: `B` the
builder's (no key), `H` the new chat's (it refuses a request without the key). The
packaged rust host runs the chain, driven over `web_rest`. The builder's lines are
E31's: `creds: get`, and E30's `create` line on itself (`creator_lines`) for the
chat and provider bundles, the provider's `can` forced to `creds: get`.

  1. the builder answers `create_agent hour_ai` (its `can`: `memory`, `creds: get`),
     then `create_agent hour_ai_prov` under it (keyed, `credentials` on `creds`),
     then `done`; a tick to `hour_ai` completes on `H` WITH the key, no request
     refused;
  2. the builder rewrites `hour_ai`'s `can` without the key line, then `done`; a tick
     fails `no agent 'creds'` and reaches `H` not at all (the GPU run's failure);
  3. the builder rewrites it with the line kept, then `done`; a tick completes on
     `H` with the key again.
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

from helpers import creds
from helpers.ai import CHAT_MODULE
from helpers.host.passes import run_pass
from helpers.host.driver import HostKernel, ensure_runtime, runtime
from helpers.lines import ALL, PROVIDER, create as line_create
from helpers.verdict import verdict
from mocks.vllm import VllmMock

REPO = E2E.parent
RUNTIME = os.environ.get("RUNTIME", "")
AI, AI_PROV = "ai", "ai_prov"
SUB_AI, SUB_PROV, SUB_CODE = "hour_ai", "hour_ai_prov", "hour_code"
PROV_MODULE, EXEC_MODULE = "vllm_backend.tools", "exec_bridge.tools"
MODEL = "mock-scripted"
REF = "twin"
KEY_VAR = "E167_TWIN_KEY"
KEY = "twin-" + secrets.token_hex(8)  # a test value: no model reads it
KEY_LINE = {creds.CREDS: creds.GET}
FAILED = "no agent 'creds'"


def xml(v) -> str:
    """A value in the call grammar: a map's keys are tags, a list is `<item>`s."""
    if isinstance(v, dict):
        return "".join(f"<{k}>{xml(x)}</{k}>" for k, x in v.items())
    if isinstance(v, list):
        return "".join(f"<item>{xml(x)}</item>" for x in v)
    return str(v)


def call(target: str, verb: str, **args) -> str:
    body = "".join(f"<{k}>{xml(v)}</{k}>" for k, v in args.items())
    return (f'<call name="send">\n<routing><target_id>{target}</target_id></routing>\n'
            f"<payload><type>{verb}</type>{body}</payload>\n</call>")


DONE = '<call name="done"></call>'


def main() -> int:
    name, _ = runtime(REPO, RUNTIME)
    spec = ensure_runtime(REPO, name)
    os.environ[KEY_VAR] = KEY  # the STORE reads it (env backend), never a record
    b = VllmMock(model=MODEL).start()
    h = VllmMock(model=MODEL).start().require_key(KEY)
    work = Path(tempfile.mkdtemp(prefix="e2e-subai-key-twin-"))
    hk = HostKernel(spec, work).boot()
    root = hk.root_id
    bad: list[str] = []

    def need(cond: bool, what: str) -> None:
        print(f"  {'ok ' if cond else 'BAD'} {what}", flush=True)
        if not cond:
            bad.append(what)

    def short(x) -> str:
        return json.dumps(x, default=str)[:300]

    def idle(bound: float = 90) -> bool:
        """Poll (bounded) until no chat of the twin has a task in flight (`ps`)."""
        end = time.monotonic() + bound
        while time.monotonic() < end:
            if hk.quiet(AI, SUB_AI):
                return True
            time.sleep(0.05)
        return False

    def build(*script: str) -> None:
        """One builder chain: the scripted answers, then wait for it to settle."""
        start = len(b.turns())
        b.reply(*script)
        out = hk.send(AI, {"type": "send", "text": "Build the routine."})
        need(out.get("accepted") is True, f"the builder's send is accepted: {short(out)}")
        # the send is acked at once (#795): the chain is over once every scripted answer
        # was asked for and nothing of the twin runs
        end = time.monotonic() + 90
        while time.monotonic() < end and len(b.turns()) - start < len(script):
            time.sleep(0.05)
        need(idle(), "the builder's chain settles")
        time.sleep(0.5)
        need(len(b.turns()) - start == len(script),
             f"one builder request per scripted answer: {len(b.turns()) - start} "
             f"for {len(script)}")

    def tick(refused: bool = False) -> tuple[list[dict], str]:
        """A message to the new chat, as the clock's would be: `H`'s requests it
        made, and the chat's rows after it, as text. `refused`: the provider pass
        fails on the key store, and a failed pass raises out of the send."""
        start = len(h.requests)
        out = run_pass(hk, SUB_AI, {"type": "send", "text": "tick"})
        if refused:
            need(FAILED in str(out.get("error") or ""),
                 f"the tick fails on the key store ({FAILED}): {short(out)}")
        else:
            need(not out.get("error"), f"the tick is accepted: {short(out)}")
        need(idle(), "the new chat's chain settles")
        rows = (hk.send(SUB_AI, {"type": "rows"}) or {}).get("rows") or []
        return h.requests[start:], json.dumps(rows, default=str)

    def sub_can() -> dict:
        return (hk.send(SUB_AI, {"type": "reflect"}) or {}).get("can") or {}

    own = {AI: line_create(
        who=[SUB_AI, SUB_PROV, SUB_CODE],
        what={CHAT_MODULE: {"*": "*"},
              PROV_MODULE: {"*": "*", **creds.creator_line(PROV_MODULE)},
              EXEC_MODULE: {"*": "*"}})}
    can = {"memory": ALL, AI_PROV: PROVIDER}
    chat_can = {"memory": ALL, **KEY_LINE}
    try:
        r = hk.send(root, {"type": "update_agent", "id": creds.CREDS,
                           "refs": {REF: {"host": h.host, "env": {"api_key": KEY_VAR}}}})
        need(r.get("updated") is True, f"creds: the twin's ref, bound to H: {short(r)}")
        rec = hk.create(parent_id="rest", handler_module=PROV_MODULE, id=AI_PROV,
                        endpoint=b.url, model=MODEL,
                        context_window=262144, kv_slots=4, kv_overflow="evict",
                        memory_store_id="memory", can={"memory": ALL})
        need(rec.get("id") == AI_PROV, f"create {AI_PROV}: {short(rec)}")
        rec = hk.create(parent_id="rest", handler_module=CHAT_MODULE, id=AI,
                        provider_id=AI_PROV, model=MODEL, max_tokens=8192,
                        context_window=262144, memory_store_id="memory", can=can)
        need(rec.get("id") == AI, f"create {AI}: {short(rec)}")
        # the operator's lines on the builder, once it exists (E31 `_wire_builder`)
        r = hk.send(root, {"type": "update_agent", "id": AI,
                           "can": {**can, **own, **KEY_LINE}})
        need(r.get("updated") is True, f"the builder's create and key lines: {short(r)}")

        # 1 · the key line handed in the create, before the provider exists
        build(
            call("kernel", "create_agent", handler_module=CHAT_MODULE, parent_id=AI,
                 id=SUB_AI, provider_id=SUB_PROV, model=MODEL, max_tokens=8192,
                 context_window=262144, memory_store_id="memory", can=chat_can),
            call("kernel", "create_agent", handler_module=PROV_MODULE, parent_id=SUB_AI,
                 id=SUB_PROV, endpoint=h.url, model=MODEL,
                 context_window=262144, kv_slots=4, kv_overflow="evict",
                 memory_store_id="memory",
                 credentials={"store": creds.CREDS, "ref": REF}, can=KEY_LINE),
            DONE,
        )
        need(sub_can() == chat_can, f"{SUB_AI} holds the handed lines: {short(sub_can())}")
        prov = hk.send(SUB_PROV, {"type": "reflect"}) or {}
        need(prov.get("parent_id") == SUB_AI and (prov.get("can") or {}) == KEY_LINE,
             f"{SUB_PROV} is under {SUB_AI}, holding only the key line: {short(prov)}")
        reqs, rows = tick()
        turns = [q for q in reqs if q["kind"] == "turn"]
        need(bool(turns) and all(q["key"] == KEY for q in turns),
             f"the first completion carries the key: {len(turns)} turn(s)")
        need(not any(q["kind"] == "refused" for q in reqs),
             f"no request to H went without the key: {[q['kind'] for q in reqs]}")
        need(FAILED not in rows, "no turn of the new chat failed on the key store")

        # 2 · a `can` rewrite that drops the line: the key is refused
        build(call("kernel", "update_agent", id=SUB_AI, can={"memory": ALL}), DONE)
        need(sub_can() == {"memory": ALL}, f"the rewrite replaced the field: {short(sub_can())}")
        reqs, rows = tick(refused=True)
        need(not reqs, f"nothing reached H without the line: {[q['kind'] for q in reqs]}")
        need(FAILED in rows, f"the new chat's turn failed on the key store ({FAILED})")

        # 3 · the rewrite that keeps it: the key goes through again
        build(call("kernel", "update_agent", id=SUB_AI, can=chat_can), DONE)
        need(sub_can() == chat_can, f"the line is back: {short(sub_can())}")
        reqs, _ = tick()
        turns = [q for q in reqs if q["kind"] == "turn"]
        need(bool(turns) and all(q["key"] == KEY for q in turns),
             f"the completion carries the key again: {len(turns)} turn(s)")
        need(not any(q["kind"] == "refused" for q in reqs),
             f"no request to H went without the key: {[q['kind'] for q in reqs]}")
    finally:
        hk.close()
        b.stop()
        h.stop()
        shutil.rmtree(work, ignore_errors=True)

    verdict(not bad, f"{RUNTIME}: {len(bad)} failed" + (f" — first: {bad[0]}" if bad else ""))
    return 0 if not bad else 1


if __name__ == "__main__":
    sys.exit(main())
