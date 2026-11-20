"""kernel/chain_restart — a chain in flight survives a kernel KILL (#726, #733). NO MODEL.

Two local OpenAI-compatible servers (`e2e/mocks/vllm.py`) play the model of a LEAD and
a WORKER; the kernel's own `vllm_backend` agents (ai_core) run on the PACKAGED rust
host, every call from the test through `web_rest`. An `exec_bridge` agent `ex` runs the
slow job. The kernel's memory store (its ledger and the AIs' rows) is a `sqlite_store`
(`stores/memory.db`: the `memory` record is rewritten while the kernel is stopped), so
what was flushed outlives the process.

  1. the LEAD's chain: it runs a 30 s job on `ex` (`run`): the call is out, its route
     waits on it;
  2. the WORKER sends the lead a message: the lead's chain is alive, so it is HELD;
     `status` names the worker in `pending_from`;
  3. the kernel process group is KILLED (SIGKILL: no hook, no drain) and a NEW kernel
     booted on the same folder.

Checks: the kernel's own rows in the store hold a `settle` for both cut calls
(`cancelled`, the restart's error: dead letters) and every call of the first life is
settled; the lead's first request after the
restart tells it its job's reply will not come (`[reply to <pid> · ex run] … dead letter
<pid>; resend or drop`) and that the worker's held message was cut; the worker's tells
it its message to the lead died (`[reply to <pid> · lead send] …`, the same pid); the
lead's chain goes on: it resends the job and its next request carries the job's output.
"""

from __future__ import annotations

import json
import os
import re
import secrets
import shlex
import shutil
import sys
import tempfile
import time
from pathlib import Path

import yaml

E2E = next(d for d in Path(__file__).resolve().parents if (d / ".e2e-root").exists())
sys.path.insert(0, str(E2E))

from helpers import oslayer
from helpers.host.driver import HostKernel, ensure_runtime, runtime
from helpers.verdict import verdict
from mocks.vllm import VllmMock

REPO = E2E.parent
RUNTIME = os.environ.get("RUNTIME", "")
LEAD, WORKER, EX = "lead", "worker", "ex"
TOKEN = secrets.token_hex(4)
RECALL_MARK = "rebuilding your own working context"
DB = "stores/memory.db"
PY = sys.executable
# the exec allowlist matches the shell-joined line, the interpreter quoted as shlex does
ALLOWED = [re.escape(shlex.quote(PY)) + r" -c .*"]
SLOW = "__import__('time').sleep(30)"
RESUMED = f"print('resumed-{TOKEN}')"


def last_user(req: dict) -> str:
    for m in reversed(req.get("messages") or []):
        if m.get("role") == "user":
            c = m.get("content")
            return c if isinstance(c, str) else json.dumps(c)
    return ""


def run_call(program: str) -> str:
    return (f'<call name="send">\n<routing><target_id>{EX}</target_id></routing>\n'
            f"<payload><type>run</type><command>{PY}</command>"
            f"<args><item>-c</item><item>{program}</item></args><wait_s>60</wait_s></payload>\n</call>")


def main() -> int:
    name, _ = runtime(REPO, RUNTIME)
    spec = ensure_runtime(REPO, name)
    lead_m = VllmMock(model="mock-lead").start()
    work_m = VllmMock(model="mock-worker").start()
    work = Path(tempfile.mkdtemp(prefix="e2e-chain-restart-"))
    hk = HostKernel(spec, work, reach=[LEAD, WORKER, EX, f"{LEAD}_prov", f"{WORKER}_prov", "memory"]).boot()
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

    def asked(m: VllmMock, needle: str, bound: float = 90) -> str:
        """The last user message of the first request carrying `needle`, waited for."""
        end = time.monotonic() + bound
        while time.monotonic() < end:
            for t in m.turns():
                u = last_user(t["body"])
                if needle in u and RECALL_MARK not in u:
                    return u
            time.sleep(0.1)
        return ""

    def until(what: str, cond, bound: float = 30) -> bool:
        end = time.monotonic() + bound
        while time.monotonic() < end:
            if cond():
                return True
            time.sleep(0.1)
        print(f"  (waited {bound}s for {what})", flush=True)
        return False

    def idle(bound: float = 90) -> bool:
        return until("the chains to settle", lambda: not [
            t for t in hk.send("kernel", {"type": "ps"}).get("tasks") or []
            if t.get("agent") in (LEAD, WORKER, EX)], bound)

    try:
        r = hk.send(root, {"type": "update_agent", "id": "memory",
                           "callable_by": {**callable_by("memory"), LEAD: "*", WORKER: "*",
                                           f"{LEAD}_prov": "*", f"{WORKER}_prov": "*"}})
        need(r.get("updated") is True, f"memory accepts the AIs: {str(r)[:200]}")
        rec = hk.create(parent_id="rest", handler_module="exec_bridge.tools", id=EX,
                        ingress_rule="allow_all_ingress", cwd=str(work), env=oslayer.child_env(),
                        allowed=ALLOWED, callable_by={"$self": "*", "$parent": "*", LEAD: "*"})
        need(rec.get("id") == EX, f"create {EX}: {str(rec)[:200]}")

        def make_ai(aid: str, mock: VllmMock, can_call: dict, cb: dict) -> None:
            common = dict(context_window=262144, memory_store_id="memory")
            prov = f"{aid}_prov"
            rec = hk.create(parent_id="rest", handler_module="vllm_backend.tools", id=prov,
                            endpoint=mock.url, model=mock.model, kv_slots=4,
                            kv_overflow="evict", **common,
                            can_call={"$self": "*", "memory": "*"},
                            callable_by={"$self": "*", "$parent": "*", aid: "*"})
            need(rec.get("id") == prov, f"create {prov}: {str(rec)[:200]}")
            rec = hk.create(parent_id="rest", handler_module="ai_chat.tools", id=aid,
                            provider_id=prov, model=mock.model, max_tokens=8192, **common,
                            can_call={**can_call, prov: ["complete", "watch", "reflect"]}, callable_by=cb)
            need(rec.get("id") == aid, f"create {aid}: {str(rec)[:200]}")

        make_ai(LEAD, lead_m, {"$self": "*", "memory": "*", EX: "*"},
                {"$self": "*", "$parent": "*", "rest": "*", WORKER: "*"})
        make_ai(WORKER, work_m, {"$self": "*", "memory": "*", LEAD: "*"},
                {"$self": "*", "$parent": "*", "rest": "*"})

        # the kernel's ledger and the AIs' rows go to a durable store
        hk.close()
        rec_path = work / ".fantastic/agents/web/agents/rest/agents/memory/agent.yaml"
        rec = yaml.safe_load(rec_path.read_text())
        rec["handler_module"] = "sqlite_store.tools"
        rec["path"] = DB
        rec_path.write_text(yaml.safe_dump(rec, sort_keys=False))
        hk.boot()
        refl = hk.send("memory", {"type": "reflect"})
        need(refl.get("durable") is True and refl.get("path") == DB,
             f"the kernel's ledger store is a sqlite_store: {str(refl)[:200]}")

        # 1 · the lead's chain: a slow job is out
        def lead_before(req: dict) -> str | None:
            u = last_user(req)
            if RECALL_MARK in u:
                return "The head is fine as it is."
            if f"Run the slow job ({TOKEN})" in u:
                return run_call(SLOW)
            return None

        lead_m.responder = lead_before
        r = hk.send(LEAD, {"type": "send", "text": f"Run the slow job ({TOKEN})."}, timeout=60)
        need(not r.get("error"), f"the lead takes the task: {str(r)[:200]}")

        def job_running() -> bool:
            jobs = hk.send(EX, {"type": "status"}).get("jobs") or []
            return any(j.get("status") == "running" for j in jobs)

        need(until("the slow job", job_running), "the lead's slow job is running on ex")

        # 2 · the worker's message to the lead is held on the lead's chain
        work_m.responder = lambda req: (
            "The head is fine as it is." if RECALL_MARK in last_user(req)
            else (f'<call name="send">\n<routing><target_id>{LEAD}</target_id></routing>\n'
                  f"<payload><type>send</type><text>QUESTION-{TOKEN}</text></payload>\n</call>")
            if f"Ask the lead ({TOKEN})" in last_user(req) else None)
        r = hk.send(WORKER, {"type": "send", "text": f"Ask the lead ({TOKEN})."}, timeout=60)
        need(not r.get("error"), f"the worker takes the task: {str(r)[:200]}")
        need(until("the held message", lambda: WORKER in (
            hk.send(LEAD, {"type": "status"}).get("pending_from") or [])),
             "the worker's message is held on the lead's chain (status names it)")
        # the flusher writes the call rows a beat after they are made
        time.sleep(1.5)

        # 3 · KILL the kernel; a new one on the same folder
        oslayer.kill_group(hk.proc.pid)
        try:
            hk.proc.wait(timeout=10)
        except Exception:  # noqa: BLE001 — named below
            pass
        need(hk.proc.poll() is not None, "the first kernel process is killed")
        hk.close()
        n_lead, n_work = len(lead_m.turns()), len(work_m.turns())

        # a restarted agent first asks its model to rebuild its working context from
        # the rows it read back: answered apart, it takes no scripted answer
        def lead_after(req: dict) -> str | None:
            u = last_user(req)
            if RECALL_MARK in u:
                return "The head is fine as it is."
            if "[restart]" in u:
                return run_call(RESUMED)
            return None

        def work_after(req: dict) -> str | None:
            return "The head is fine as it is." if RECALL_MARK in last_user(req) else None

        lead_m.responder = lead_after
        work_m.responder = work_after
        hk.boot()
        need((work / DB).is_file(), f"the ledger file is on disk: {DB}")

        # the lead is told: its job's reply will not come, the held message was cut
        lead_told = asked(lead_m, "[restart]")
        print(f"  lead was told: {lead_told[:600]!r}", flush=True)
        job = re.search(r"\[reply to (\d+) · ex run\] no reply will come → dead letter (\d+); resend or drop",
                        lead_told)
        need(job is not None and job.group(1) == job.group(2),
             "the lead is told its job's reply will not come: a dead letter to resend or drop")
        held = re.search(rf"\[from {WORKER} · dead letter (\d+)\] a message to you was cut", lead_told)
        need(held is not None, "the lead is told the worker's held message was cut")
        # the held sender is told
        work_told = asked(work_m, "[restart]")
        print(f"  worker was told: {work_told[:400]!r}", flush=True)
        sent = re.search(rf"\[reply to (\d+) · {LEAD} send\] no reply will come → dead letter (\d+)",
                         work_told)
        need(sent is not None and held is not None and sent.group(1) == held.group(1),
             "the worker (the held sender) is told its message died, under the same id")

        # THE RECORDS (the `ledger` verb is scoped to its caller, here the `rest` leg): the
        # kernel's own rows in the store hold a `settle` for both cut calls, `cancelled`
        # with the restart's error, and every call of the first life has its settle
        time.sleep(1.0)  # the flusher writes a beat after the start
        tables = hk.send("memory", {"type": "dump"}).get("tables") or {}
        krows = [r.get("row") or {} for r in tables.get("rows") or []
                 if str(r.get("owner", "")).endswith("|kernel")]
        settles = {r.get("parent"): r for r in krows if r.get("kind") == "settle"}
        for what, m in (("the job", job), ("the held message", held)):
            st = settles.get(int(m.group(1))) if m else None
            oc = (st or {}).get("outcome") or {}
            need(oc.get("status") == "cancelled" and "cut by the restart" in str(oc.get("error")),
                 f"{what} is a dead letter on the kernel's records: {str(st)[:300]}")
        first_ids = [int(m.group(1)) for m in (job, held) if m]
        stale = [r.get("id") for r in krows
                 if r.get("kind") == "call" and not r.get("internal") and "attempts" in r
                 and first_ids and r.get("id", 0) <= max(first_ids) and r.get("id") not in settles]
        need(bool(krows) and not stale,
             f"every call of the first life is settled on the records: {len(krows)} kernel rows, unsettled {stale[:10]}")

        # the lead's chain goes on: the resent job is answered
        back = asked(lead_m, f"resumed-{TOKEN}")
        need(bool(back) and "ex run" in back,
             f"the lead's chain goes on: its resent job's reply reaches it: {back[:300]!r}")
        need(idle(), "the chains settle after the restart")
        after_l = [u for u in (last_user(t["body"]) for t in lead_m.turns()[n_lead:])
                   if RECALL_MARK not in u]
        need(sum("[restart]" in u for u in after_l) == 1, "the lead is told once")
        after_w = [u for u in (last_user(t["body"]) for t in work_m.turns()[n_work:])
                   if RECALL_MARK not in u]
        need(sum("[restart]" in u for u in after_w) == 1, "the worker is told once")
    finally:
        hk.close()
        lead_m.stop()
        work_m.stop()
        shutil.rmtree(work, ignore_errors=True)

    verdict(not bad, f"{RUNTIME}: {len(bad)} failed" + (f" — first: {bad[0]}" if bad else ""))
    return 0 if not bad else 1


if __name__ == "__main__":
    sys.exit(main())
