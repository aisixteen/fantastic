"""kernel/await_set — a lead waits for a SET of workers and wakes ONCE. NO MODEL.

Four local OpenAI-compatible servers (`e2e/mocks/vllm.py`) play the model of a lead
and of three workers; the kernel's own `vllm_backend` agents (ai_core) run on the
PACKAGED host of `RUNTIME` (python | rust), every call from the test through
`web_rest`. Tool calling is the prompt-taught `<call name>` tag grammar.

The lead's script, one model answer per request:

  1. `await_set {ids:[w1,w2,w3], timeout_s:60, label:E128}` on the kernel: it
     answers AT ONCE with a `wait_id`, which is the next request's last user turn;
  2. a plain answer + `done` — the lead is idle;
  3. the combined message reaches the lead as ONE more request: a plain answer +
     `done`.

Between 2 and 3 the TEST sends each worker its task (so the lead is idle and the
combined message is a clean turn of its own on both runtimes).

Each worker's script: a `send` of `type: report` (`state: done`, `result: R<n>`)
to the lead — answered `{held: true, wait_id}`, the report is HELD — then `done`.

Checks: the `await_set` reply carries the `wait_id`; exactly ONE lead request
carries a combined message — `wait_id`, `label`, `done` (w1, w2, w3 in `ids` order
with R1..R3), `missing: []`, `timed_out: false`; no lead request's last user turn
is a bare report; the lead's request count equals its script length; each worker
made exactly its two requests.
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
LEAD = "lead"
WORKERS = ["w1", "w2", "w3"]
LABEL = "E128"
TOKEN = "E128-" + secrets.token_hex(4)


def last_user(req: dict) -> str:
    """The last user message of a chat request, as text."""
    for m in reversed(req.get("messages") or []):
        if m.get("role") == "user":
            c = m.get("content")
            return c if isinstance(c, str) else json.dumps(c)
    return ""


def combined(text: str) -> dict | None:
    """The combined message's JSON body inside a user turn, if there is one."""
    dec = json.JSONDecoder()
    for i, ch in enumerate(text):
        if ch != "{":
            continue
        try:
            obj, _ = dec.raw_decode(text[i:])
        except ValueError:
            continue
        if isinstance(obj, dict) and "wait_id" in obj and "done" in obj:
            return obj
    return None


def main() -> int:
    name, _ = runtime(REPO, RUNTIME)
    spec = ensure_runtime(REPO, name)
    lead_m = VllmMock(model="mock-lead").start()
    work_m = {w: VllmMock(model=f"mock-{w}").start() for w in WORKERS}
    work = Path(tempfile.mkdtemp(prefix="e2e-await-set-"))
    hk = HostKernel(spec, work, reach=[LEAD, *WORKERS]).boot()
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

    def turns_at_least(m: VllmMock, n: int, bound: float = 60) -> list[dict]:
        end = time.monotonic() + bound
        while time.monotonic() < end:
            got = m.turns()
            if len(got) >= n:
                return got
            time.sleep(0.05)
        return m.turns()

    def idle(bound: float = 60) -> bool:
        """Poll (bounded) until none of the four AIs has a task in flight."""
        end = time.monotonic() + bound
        while time.monotonic() < end:
            tasks = hk.send("kernel", {"type": "ps"}).get("tasks") or []
            if not any(t.get("agent") in (LEAD, *WORKERS) for t in tasks):
                return True
            time.sleep(0.05)
        return False

    ids_xml = "".join(f"<item>{w}</item>" for w in WORKERS)
    lead_script = [
        # 1 · open the wait BEFORE anyone is asked to work
        (f'Opening the wait first.\n<call name="send">\n<target_id>{root}</target_id>\n'
         f'<type>await_set</type>\n<ids>{ids_xml}</ids>\n<timeout_s>60</timeout_s>\n'
         f'<label>{LABEL}</label>\n</call>'),
        # 2 · nothing more to do until the one message arrives
        'Waiting for the workers.\n<call name="done"></call>',
        # 3 · the combined message arrived
        'All three have reported.\n<call name="done"></call>',
    ]
    try:
        mem = callable_by("memory")
        r = hk.send(root, {"type": "update_agent", "id": "memory",
                           "callable_by": {**mem, LEAD: "*", **{w: "*" for w in WORKERS}}})
        need(r.get("updated") is True, f"memory accepts the AIs: {str(r)[:200]}")
        r = hk.send(root, {"type": "update_agent", "id": root,
                           "callable_by": {**callable_by(root), LEAD: ["await_set"]}})
        need(r.get("updated") is True, f"the root accepts the lead's await_set: {str(r)[:200]}")
        common = dict(max_tokens=8192, context_window=262144, kv_slots=4,
                      kv_overflow="evict", memory_store_id="memory")
        for i, w in enumerate(WORKERS):
            rec = hk.create(parent_id="rest", handler_module="vllm_backend.tools", id=w,
                            endpoint=work_m[w].url, model=f"mock-{w}", **common,
                            can_call={"$self": "*", "memory": "*", LEAD: ["report"]},
                            callable_by={"$self": "*", "$parent": "*", "rest": "*"})
            need(rec.get("id") == w, f"create {w}: {str(rec)[:200]}")
        rec = hk.create(parent_id="rest", handler_module="vllm_backend.tools", id=LEAD,
                        endpoint=lead_m.url, model="mock-lead", **common,
                        can_call={"$self": "*", "memory": "*", root: ["await_set"],
                                  **{w: "*" for w in WORKERS}},
                        callable_by={"$self": "*", "$parent": "*", "rest": "*",
                                     **{w: ["report"] for w in WORKERS}})
        need(rec.get("id") == LEAD, f"create {LEAD}: {str(rec)[:200]}")
        for w in WORKERS:  # the lead may call each worker: both ends say yes
            cb = callable_by(w)
            hk.send(root, {"type": "update_agent", "id": w, "callable_by": {**cb, LEAD: "*"}})

        lead_m.reply(*lead_script)
        for i, w in enumerate(WORKERS):
            work_m[w].reply(
                f'<call name="send">\n<target_id>{LEAD}</target_id>\n<type>report</type>\n'
                f'<state>done</state>\n<result>R{i + 1}</result>\n</call>',
                '<call name="done"></call>')
        out = hk.send(LEAD, {"type": "send", "text": f"Wait for the three workers ({TOKEN})."})
        need(not out.get("error"), f"the send is accepted: {str(out)[:200]}")
        need(len(turns_at_least(lead_m, 2)) >= 2 and idle(),
             "the lead opened its wait and is idle (no AI task in `ps`)")
        # the lead is idle: the three workers are asked, one after the other
        for i, w in enumerate(WORKERS):
            out = hk.send(w, {"type": "send", "text": f"Do task {i + 1} and report."})
            need(not out.get("error"), f"{w} ran its task: {str(out)[:120]}")
        turns_at_least(lead_m, len(lead_script))
        need(idle(), "the chain settles (no AI task left in `ps`)")
        time.sleep(0.5)  # a stray extra turn would show here
        turns = lead_m.turns()
        u = [last_user(t["body"]) for t in turns]
        for i, t in enumerate(u):
            print(f"  ── lead request {i + 1}: last user turn ──\n"
                  + "\n".join("     " + ln for ln in t[:600].splitlines()), flush=True)

        need(len(turns) == len(lead_script),
             f"the lead made one request per scripted answer: {len(turns)} for {len(lead_script)}")
        # the await_set call answered at once, with its wait_id
        need(len(u) > 1 and "[reply to " in u[1] and "await_set" in u[1] and '<wait_id>' in u[1],
             f"request 2 carries the await_set reply with a wait_id: {u[1][:200] if len(u) > 1 else ''!r}")
        # exactly one combined message
        hits = [(i, combined(t)) for i, t in enumerate(u) if combined(t)]
        need(len(hits) == 1, f"exactly ONE lead request carries a combined message: {[i + 1 for i, _ in hits]}")
        body = hits[0][1] if hits else {}
        need(isinstance(body.get("wait_id"), int), f"it carries the wait_id: {body.get('wait_id')!r}")
        need(body.get("label") == LABEL, f"label {LABEL}: {body.get('label')!r}")
        need(body.get("timed_out") is False and body.get("missing") == [],
             f"nobody missing, no timeout: {body.get('missing')!r} {body.get('timed_out')!r}")
        done = body.get("done") or []
        need([d.get("id") for d in done] == WORKERS
             and [d.get("result") for d in done] == ["R1", "R2", "R3"]
             and all(d.get("state") == "done" for d in done),
             f"done holds w1..w3 in order with R1..R3: {done!r}")
        # in the last request's whole history: ONE combined message, and no report
        hist = [m.get("content") if isinstance(m.get("content"), str) else json.dumps(m.get("content"))
                for m in (turns[-1]["body"].get("messages") or []) if m.get("role") == "user"]
        need(sum(1 for t in hist if combined(t)) == 1,
             f"the lead's history holds the combined message once: {sum(1 for t in hist if combined(t))}")
        need(not any(t.lstrip().startswith("[from w") for t in hist),
             "no report was delivered to the lead as a message of its own")
        for i, w in enumerate(WORKERS):
            n = len(turns_at_least(work_m[w], 2))
            need(n == 2, f"{w} made exactly its two requests (report, done): {n}")
            need(n >= 2 and "<held>true</held>" in last_user(work_m[w].turns()[1]["body"]),
                 f"{w}'s report was answered as held")
    finally:
        hk.close()
        lead_m.stop()
        for m in work_m.values():
            m.stop()
        shutil.rmtree(work, ignore_errors=True)

    verdict(not bad, f"{RUNTIME}: {len(bad)} failed" + (f" — first: {bad[0]}" if bad else ""))
    return 0 if not bad else 1


if __name__ == "__main__":
    sys.exit(main())
