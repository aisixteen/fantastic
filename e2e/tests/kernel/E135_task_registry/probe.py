"""kernel/task_registry — the task registry on a ram_store, driven by SCRIPTED models. NO MODEL.

A local OpenAI-compatible server (`e2e/mocks/vllm.py`) plays every agent's model; the
agents are the kernel's own `vllm_backend` agents (ai_core) on the PACKAGED host of
`RUNTIME` (python | rust), every call from the test through `web_rest`. Both registries
are OFF by default: the probe starts the host with `FANTASTIC_AGENTIC_RESOURCES=1` and
`FANTASTIC_TASK_REGISTRY=1`. The task registry (`tasks`) keeps its tables in the
`ram_store` agent `tstore` named by its record (`task_store_id`); `can_call` on the
store and the store's `callable_by` are wired both ends, as for memory.

The cast: the lead `L0` (role `lead`: spawn on `agentic_resources` and the write verbs on
`tasks`), a sub-lead `SA` (same role, never prompted: it only HOLDS a task) and a worker
`WA` (role `worker`: `report` on `agentic_resources` and nothing on `tasks`).

SCRIPT — `L0` (the test sends it its first message once `tasks` lets it in):
  1. `add T0` (source `gh:aisixteen/fantastic#429`, holder L0);
  2. `spawn SA` and `spawn WA` through `agentic_resources` (WA starts at once);
  3. `add T1` (parent T0, source `jira:KEY-5`, update_to `gh:aisixteen/fantastic#429`);
  4. `assign T1` to SA, then `reassign T1` to the subtree rooted at SA;
  5. when WA has reported, `report T1 from=WA` and `status T1 done`, on its behalf.
`WA` first tries `tasks add` itself (refused by the gate), then `report`s done to its
parent through `agentic_resources` and ends.

Checks: `list` shows T0 and T1 sorted and `get T0` lists T1 as a child; sources are
stored verbatim and `update_to` defaults to `source`; T1's history is two steps
(null to SA by L0, SA to the subtree by L0) and its holder is `{subtree: SA}`; its last
report is WA's, its status done; the store `dump` holds one `kind:task` row per write
(6), all under the registry's key; WA's direct write was refused `unauthorized` and
created no task; the registry made no call but to the store (no other row appears).
"""

from __future__ import annotations

import json
import os
import re
import shutil
import sys
import tempfile
import threading
import time
from pathlib import Path

E2E = next(d for d in Path(__file__).resolve().parents if (d / ".e2e-root").exists())
sys.path.insert(0, str(E2E))

from helpers.host.driver import HostKernel, ensure_runtime, runtime
from helpers.verdict import verdict
from mocks.vllm import VllmMock

REPO = E2E.parent
RUNTIME = os.environ.get("RUNTIME", "")
AR, TR, STORE = "agentic", "tasks", "tstore"
MODEL = "mock-scripted"
KEY = re.compile(r"\b(L0|WA):")
WRITES = ["add", "assign", "reassign", "status", "report", "list", "get"]

LEAD = ("---\nname: lead\npurpose: Runs a branch and keeps its tasks in the registry.\n"
        "tier: fast\ngrants:\n  $registry: [roles, spawn, get_count, report]\n"
        f"  {TR}: [{', '.join(WRITES)}]\n---\nYou run one branch.\n")
WORKER = ("---\nname: worker\npurpose: Does one small piece of the work and reports it.\n"
          "tier: fast\ngrants:\n  $registry: [report]\n---\nYou do one piece and report it.\n")


def text_of(m: dict) -> str:
    c = m.get("content")
    return c if isinstance(c, str) else json.dumps(c)


def call(target: str, verb: str, **args) -> str:
    """One `<call name="send">`; a dict value rides as child tags."""
    def tag(k, v):
        if isinstance(v, dict):
            return f"<{k}>" + "".join(tag(a, b) for a, b in v.items()) + f"</{k}>"
        return f"<{k}>{v}</{k}>"
    return (f'<call name="send">\n<target_id>{target}</target_id>\n<type>{verb}</type>\n'
            + "".join(tag(k, v) for k, v in args.items()) + "\n</call>")


DONE = 'Finished.\n<call name="done"></call>'
SRC_T0 = "gh:aisixteen/fantastic#429"


def main() -> int:
    name, _ = runtime(REPO, RUNTIME)
    spec = ensure_runtime(REPO, name)
    m = VllmMock(model=MODEL).start()
    work = Path(tempfile.mkdtemp(prefix="e2e-task-registry-"))
    roles_dir = work / "roles"
    roles_dir.mkdir()
    (roles_dir / "lead.md").write_text(LEAD)
    (roles_dir / "worker.md").write_text(WORKER)
    hk = HostKernel(spec, work, reach=[AR, TR, STORE])
    hk.env["FANTASTIC_AGENTIC_RESOURCES"] = "1"
    hk.env["FANTASTIC_TASK_REGISTRY"] = "1"
    hk.boot()
    root = hk.root_id
    bad: list[str] = []

    def need(cond: bool, what: str) -> None:
        print(f"  {'ok ' if cond else 'BAD'} {what}", flush=True)
        if not cond:
            bad.append(what)

    def records() -> dict[str, dict]:
        r = hk.send("kernel", {"type": "list_agents"})
        return {a["id"]: a for a in r.get("agents") or []}

    def named(label: str) -> str | None:
        return next((i for i, r in records().items() if r.get("name") == label), None)

    def until(cond, bound: float = 90) -> bool:
        end = time.monotonic() + bound
        while time.monotonic() < end:
            if cond():
                return True
            time.sleep(0.1)
        return False

    def cb_of(aid: str) -> dict:
        return dict((records().get(aid) or {}).get("callable_by") or {})

    seen: dict[str, list[str]] = {}
    lock = threading.Lock()
    wa_reported = threading.Event()
    ids: dict[str, str] = {}

    def sa() -> str:
        return named("SA") or ""

    def wa() -> str:
        return named("WA") or ""

    def l0_report() -> str:
        wa_reported.wait(60)
        return call(TR, "report", id="T1", **{"from": wa()}, state="done", result="leaf ok")

    def wa_report() -> str:
        wa_reported.set()
        return call(AR, "report", state="done", result="leaf ok")

    scripts: dict[str, list] = {
        "L0": [lambda: call(TR, "add", id="T0", source=SRC_T0, holder=ids["L0"]),
               call(AR, "spawn", role="lead", name="SA"),
               call(AR, "spawn", role="worker", name="WA", task="WA: do the leaf"),
               call(TR, "add", id="T1", parent="T0", source="jira:KEY-5", update_to=SRC_T0),
               lambda: call(TR, "assign", id="T1", holder=sa()),
               lambda: call(TR, "reassign", id="T1", holder={"subtree": sa()}),
               l0_report,
               call(TR, "status", id="T1", status="done"),
               DONE],
        "WA": [call(TR, "add", id="W_DIRECT", source="never"), wa_report, DONE],
    }

    def responder(req: dict):
        msgs = req.get("messages") or []
        first = next((text_of(x) for x in msgs if x.get("role") == "user"), "")
        km = KEY.search(first)
        if not km:
            return None
        key = km.group(1)
        last = next((text_of(x) for x in reversed(msgs) if x.get("role") == "user"), "")
        with lock:
            seen.setdefault(key, []).append(last)
            queue = scripts.get(key) or []
            step = queue.pop(0) if queue else DONE
        return step() if callable(step) else step

    m.responder = responder

    def spent(*keys: str) -> bool:
        return all(not scripts[k] for k in keys)

    def settled(*aids: str, bound: float = 90) -> bool:
        end = time.monotonic() + bound
        while time.monotonic() < end:
            tasks = hk.send("kernel", {"type": "ps"}).get("tasks") or []
            if not any(t.get("agent") in aids for t in tasks):
                return True
            time.sleep(0.1)
        return False

    tier = {"handler_module": "vllm_backend.tools", "endpoint": m.url, "model": MODEL,
            "max_tokens": 8192, "context_window": 262144, "kv_slots": 8,
            "kv_overflow": "evict", "memory_store_id": "memory"}
    try:
        # the store and the registry, wired both ends (as for memory)
        rec = hk.create(parent_id="rest", handler_module="ram_store.tools", id=STORE,
                        callable_by={TR: "*", "$parent": "*"})
        need(rec.get("id") == STORE, f"create the store agent: {str(rec)[:200]}")
        rec = hk.create(parent_id="rest", handler_module="task_registry.tools", id=TR,
                        task_store_id=STORE, can_call={STORE: "*"},
                        callable_by={"$self": "*", "$parent": "*"})
        need(rec.get("id") == TR, f"create the task registry: {str(rec)[:200]}")
        refl = hk.send(TR, {"type": "reflect"})
        need(refl.get("enabled") is True, f"the registry is enabled by its store: {refl}")
        rec = hk.create(parent_id="rest", handler_module="agentic_resources.tools", id=AR,
                        role_dirs=[str(roles_dir)], tiers={"fast": tier, "lead": tier},
                        callable_by={"$self": "*", "$parent": "*"})
        need(rec.get("id") == AR, f"create the agentic registry: {str(rec)[:200]}")

        # L0: spawned by the test, no first message yet; the task registry lets it in
        out = hk.send(AR, {"type": "spawn", "role": "lead", "name": "L0"})
        l0 = out.get("id")
        need(bool(l0), f"the test spawns the lead L0: {str(out)[:200]}")
        ids["L0"] = l0
        hk.send(root, {"type": "update_agent", "id": TR,
                       "callable_by": {**cb_of(TR), l0: "*"}})
        hk.send(root, {"type": "update_agent", "id": l0,
                       "callable_by": {**cb_of(l0), "rest": "*"}})
        out = hk.send(l0, {"type": "send", "text": "L0: keep the task tree for this work"})
        need(not out.get("error"), f"L0 runs: {str(out)[:120]}")
        need(until(lambda: spent("L0", "WA"), 120) and settled(l0, wa()),
             "L0's and WA's scripts run to their end and settle")

        # ── the registry, read back ──────────────────────────────────────────
        lst = hk.send(TR, {"type": "list"})
        tasks = lst.get("tasks") or []
        need([t.get("id") for t in tasks] == ["T0", "T1"], f"list shows the tree: {lst}")
        t0 = hk.send(TR, {"type": "get", "id": "T0"})
        t1 = hk.send(TR, {"type": "get", "id": "T1"})
        need(t0.get("children") == ["T1"] and t1.get("parent") == "T0",
             f"get T0 lists T1 as its child: {t0.get('children')}")
        need(t0.get("source") == SRC_T0 and t0.get("update_to") == SRC_T0
             and t0.get("holder") == l0,
             f"source and holder are stored as given, update_to defaults to source: {t0}")
        need(t1.get("source") == "jira:KEY-5" and t1.get("update_to") == SRC_T0,
             f"T1's source is opaque and its update_to is its own: {t1}")
        hist = [(h.get("from"), h.get("to"), h.get("by")) for h in t1.get("history") or []]
        need(hist == [(None, sa(), l0), (sa(), {"subtree": sa()}, l0)],
             f"T1's history keeps both steps: {hist}")
        need(t1.get("holder") == {"subtree": sa()} and t1.get("status") == "done",
             f"T1 is held by SA's subtree and done: {t1.get('holder')} {t1.get('status')}")
        rep = t1.get("report") or {}
        need(rep.get("from") == wa() and rep.get("state") == "done"
             and rep.get("result") == "leaf ok" and rep.get("at"),
             f"T1's last report is the worker's, written by its lead: {rep}")

        # ── the store, and the gate ──────────────────────────────────────────
        rows = (hk.send(STORE, {"type": "dump"}).get("tables") or {}).get("rows") or []
        mine = [r for r in rows if (r.get("row") or {}).get("kind") == "task"]
        need(len(mine) == 6, f"one task row per write (add x2, assign, reassign, report, "
                             f"status): {len(mine)}")
        need(len({r.get("owner") for r in mine}) == 1
             and str(mine[0].get("owner")).endswith(f"|{TR}"),
             f"all rows are keyed by the registry: {[r.get('owner') for r in mine][:2]}")
        need(len(rows) == len(mine), "the store holds nothing but the registry's rows")
        wa_seen = (seen.get("WA") or []) + [""]
        need("unauthorized" in wa_seen[1], f"WA's direct write is refused by the gate: "
                                           f"{wa_seen[1][:200]!r}")
        need(not any(t.get("id") == "W_DIRECT" for t in tasks), "the refused write made no task")
        recs = records()
        need(all("task" not in (recs.get(a) or {}) for a in (l0, sa(), wa())),
             "no agent record carries a task line")
    finally:
        hk.close()
        m.stop()
        shutil.rmtree(work, ignore_errors=True)

    verdict(not bad, f"{RUNTIME}: {len(bad)} failed" + (f" — first: {bad[0]}" if bad else ""))
    return 0 if not bad else 1


if __name__ == "__main__":
    sys.exit(main())
