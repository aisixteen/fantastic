"""kernel/reports_reach_lead — three workers report to a lead, each report a message of
its own, read once (#795; it was `await_set`, removed by the owner: Q1). NO MODEL.

Four local OpenAI-compatible servers (`e2e/mocks/vllm.py`) play the model of a LEAD and
of three WORKERS; the kernel's own AI agents (`ai_chat.tools` chats, each naming its own
`vllm_backend` provider, #521) run on the PACKAGED rust host, every call from the test
through `web_rest`. Tool calling is the prompt-taught `<call name>` tag grammar. Each
AI's own `can` lines are its only reach: the workers hold `send` on the lead.

EVERYTHING ASYNC: nothing waits for a set and nothing is held. The lead is told to expect
three reports, answers plainly + `done` and is idle. The TEST then asks the three workers
at once; each worker's model `send`s its report (`REPORT-<token> w<n> R<n>`) to the lead,
then `done`. Each report is stored on the lead and acked at once; each arrival wakes the
lead, and the reports that land while a pass runs are read by exactly one next pass
(coalesced, no batch hold).

Checks: every send is accepted; each worker made exactly ONE request (the ack of its
`send` to an AI is stored, never a wake: Q2); the lead's last history holds each report
exactly once, as a message from its worker (`[from w<n> · msg …]`); every lead pass after
the kick reads a report for the first time (no wake without news: a report that lands
during a pass is read once by the next one); the lead ran 1 to 3 passes after the kick;
nothing is left in `ps`.
"""

from __future__ import annotations

import json
import os
import re
import secrets
import shutil
import sys
import tempfile
import threading
import time
from pathlib import Path

E2E = next(d for d in Path(__file__).resolve().parents if (d / ".e2e-root").exists())
sys.path.insert(0, str(E2E))

from helpers.host.driver import HostKernel, ensure_runtime, runtime
from helpers.lines import ALL, PROVIDER, call
from helpers.verdict import verdict
from mocks.vllm import VllmMock

REPO = E2E.parent
RUNTIME = os.environ.get("RUNTIME", "")
LEAD = "lead"
WORKERS = ["w1", "w2", "w3"]
TOKEN = "E128-" + secrets.token_hex(4)
DONE = '<call name="done"></call>'
PROVS = {a: f"{a}_prov" for a in (LEAD, *WORKERS)}  # each chat's provider
# the local-time head each message opens with (#653): the request is read past it
TS_HEAD = re.compile(r"^\[\d{4}-\d\d-\d\d \d\d:\d\d [+-]\d\d:\d\d\]\n")


def users(req: dict) -> list[str]:
    """Every user message of a chat request, as text past its time head."""
    out = []
    for m in req.get("messages") or []:
        if m.get("role") == "user":
            c = m.get("content")
            out.append(TS_HEAD.sub("", c if isinstance(c, str) else json.dumps(c)))
    return out


def fresh(req: dict) -> list[str]:
    """The user messages after the request's last answer: what its pass reads anew."""
    msgs = req.get("messages") or []
    at = max((i for i, m in enumerate(msgs) if m.get("role") == "assistant"), default=-1)
    return [TS_HEAD.sub("", m.get("content") if isinstance(m.get("content"), str) else json.dumps(m.get("content")))
            for m in msgs[at + 1:] if m.get("role") == "user"]


def report(w: str) -> str:
    return f"REPORT-{TOKEN} {w} R{WORKERS.index(w) + 1}"


def worker_model(w: str):
    """A scripted worker: a task makes it `send` its report to the lead, then `done`."""
    answered = threading.Event()

    def respond(req: dict) -> str | None:
        if any(f"TASK-{TOKEN}" in u for u in users(req)) and not answered.is_set():
            answered.set()
            return (f'<call name="send"><routing><target_id>{LEAD}</target_id></routing>'
                    f"<payload><type>send</type><text>{report(w)}</text></payload></call>" + DONE)
        return DONE

    return respond


def main() -> int:
    name, _ = runtime(REPO, RUNTIME)
    spec = ensure_runtime(REPO, name)
    lead_m = VllmMock(model="mock-lead").start()
    work_m = {w: VllmMock(model=f"mock-{w}").start() for w in WORKERS}
    work = Path(tempfile.mkdtemp(prefix="e2e-reports-lead-"))
    hk = HostKernel(spec, work).boot()
    bad: list[str] = []

    def need(cond: bool, what: str) -> None:
        print(f"  {'ok ' if cond else 'BAD'} {what}", flush=True)
        if not cond:
            bad.append(what)

    def until(what: str, cond, bound: float = 60) -> bool:
        end = time.monotonic() + bound
        while time.monotonic() < end:
            if cond():
                return True
            time.sleep(0.05)
        print(f"  (waited {bound}s for {what})", flush=True)
        return False

    def idle() -> bool:
        return until("the AIs to be quiet", lambda: hk.quiet(LEAD, *WORKERS))

    try:
        def make(aid, mock, model, can):
            fields = dict(endpoint=mock.url, model=model,
                          context_window=262144, memory_store_id="memory")
            # a provider (the vllm_backend) and the chat (ai_chat.tools) naming it:
            # a `send` to a provider is refused (#521)
            pv = PROVS[aid]
            rec = hk.create(parent_id="rest", handler_module="vllm_backend.tools", id=pv,
                            kv_slots=4, kv_overflow="evict", can={"memory": ALL}, **fields)
            need(rec.get("id") == pv, f"create {pv}: {str(rec)[:200]}")
            rec = hk.create(parent_id="rest", handler_module="ai_chat.tools", id=aid,
                            provider_id=pv, model=model, max_tokens=8192,
                            context_window=262144, memory_store_id="memory",
                            can={**can, pv: PROVIDER})
            need(rec.get("id") == aid, f"create {aid}: {str(rec)[:200]}")

        # the lead first: the workers' lines name it
        make(LEAD, lead_m, "mock-lead", {"memory": ALL})
        for w in WORKERS:
            make(w, work_m[w], f"mock-{w}", {"memory": ALL, LEAD: call("send")})
            work_m[w].responder = worker_model(w)
        lead_m.responder = lambda req: "Noted." + DONE

        out = hk.send(LEAD, {"type": "send", "text": f"Expect three reports ({TOKEN})."})
        need(out.get("accepted") is True, f"the kick is accepted: {str(out)[:200]}")
        need(until("the lead's first pass", lambda: len(lead_m.passes()) >= 1) and idle(),
             "the lead answered the kick and is idle")
        kicked = len(lead_m.passes())

        # the three workers are asked AT ONCE: their reports land close together
        outs: dict[str, dict] = {}

        def ask(w: str) -> None:
            outs[w] = hk.send(w, {"type": "send", "text": f"TASK-{TOKEN}: do your task and report to {LEAD}."})

        threads = [threading.Thread(target=ask, args=(w,)) for w in WORKERS]
        for t in threads:
            t.start()
        for t in threads:
            t.join(120)
        for w in WORKERS:
            need((outs.get(w) or {}).get("accepted") is True, f"{w}'s task is accepted: {str(outs.get(w))[:160]}")
        need(until("every report in the lead's requests",
                   lambda: all(any(report(w) in u for t in lead_m.passes() for u in users(t["body"])) for w in WORKERS))
             and idle(), "the lead read every report and every AI is quiet")
        time.sleep(0.5)  # a stray extra pass would show here

        passes = lead_m.passes()
        after = passes[kicked:]
        for i, t in enumerate(after):
            print(f"  ── lead pass {i + 1} after the kick: new user messages ──\n"
                  + "\n".join("     " + ln for u in fresh(t["body"]) for ln in u[:300].splitlines()), flush=True)
        need(1 <= len(after) <= len(WORKERS),
             f"the lead ran 1..{len(WORKERS)} passes over the reports (coalesced, one per arrival at most): {len(after)}")
        # NO WAKE WITHOUT NEWS: every pass after the kick reads a report for the first time
        # (a report that lands during a pass is stored above its answer, and the next pass
        # reads it once, with the arrival notice)
        for i, t in enumerate(after):
            seen_before = {w for w in WORKERS for tt in after[:i] for u in users(tt["body"]) if report(w) in u}
            news = [w for w in WORKERS if w not in seen_before and any(report(w) in u for u in users(t["body"]))]
            need(bool(news), f"lead pass {i + 1} reads a report for the first time: {news}")
        hist = users(passes[-1]["body"]) if passes else []
        for w in WORKERS:
            held = [u for u in hist if report(w) in u]
            need(len(held) == 1, f"the lead's history holds {w}'s report once: {len(held)}")
            need(bool(held) and held[0].lstrip().startswith(f"[from {w} · msg "),
                 f"it is a message of its own from {w}: {held[0][:80] if held else ''!r}")
            first = next((i for i, t in enumerate(after) if any(report(w) in u for u in users(t["body"]))), None)
            need(first is not None, f"a lead pass read {w}'s report: first in pass {first}")
            n = len(work_m[w].passes())
            need(n == 1, f"{w} made exactly one request (its send's ack woke nothing): {n}")
        tasks = hk.send("kernel", {"type": "ps"}).get("tasks") or []
        mine = [t for t in tasks if t.get("agent") in (LEAD, *WORKERS, *PROVS.values())]
        need(not mine, f"nothing is left in `ps`: {mine}")
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
