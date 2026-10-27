"""kernel/agentic_resources — the roles registry, driven by SCRIPTED models. NO MODEL.

A local OpenAI-compatible server (`e2e/mocks/vllm.py`) plays every agent's model;
the agents are the kernel's own `vllm_backend` agents (ai_core) on the PACKAGED
host of `RUNTIME` (python | rust), every call from the test through `web_rest`. The
registry (`agentic_resources`) is OFF by default: the probe starts the host with
`FANTASTIC_AGENTIC_RESOURCES=1` and gives the registry agent its config on the
record (`role_dirs`, `tiers`). One server answers many agents, so the responder
picks the script by the agent's own first message (`L1:`, `L0:`, `SA:` ...: the
`task` text `spawn` sends the child). Tool calling is the prompt-taught `<call>` tag
grammar; nested values ride as child tags (`<limits><total>1</total></limits>`).

SCRIPT 1 (base) — the lead `L1` (a `project_lead`, spawned by the test):
  1. `roles`: the reply lists `writer` and the shipped `project_lead`;
  2. `spawn writer`, twice: both create an agent (the limit is 2);
  3. a third `spawn writer`: refused `limit` (scope role, running 2, max 2);
  4. `get_count role=writer`: running 2, max 2 (the registry's own count agrees);
  then it `report`s done and ends.

SCRIPT 2 (extended) — the top lead `L0`, a `project_lead`:
  1. spawns two sub-leads `SA` and `SB`, each with `limits:{total:1}` (split);
  2. `SA` spawns one worker `WA` (its quota), a second `spawn` is refused (scope
     subtree_total), then it waits for `WA` and reports one line;
  3. `SB` does the same with `WB`;
  4. each worker `report`s done with its result; `L0` waits for both sub-leads and
     reports ONE line condensing theirs.
  The waits are the scripted model's own (the responder blocks that turn until the
  child's scripted `report` step has run) — what `await_set` (#426) will be for a real
  model. `report` only messages the parent: nothing is stored on any record (the task
  tree is `task_registry`'s, E135), so the probe keeps each scripted result itself.
  Checks: every scripted agent reported done and the top result carries both parts,
  the tree nests the agents under their callers (`reflect tree=all`), `get_count`
  counts only the ACTIVE agents (a `done` report pauses the reporter and frees its
  slot, so only the two idle writers of script 1 still count), and EVERY agent is
  still there at the end: nothing was cleaned up (`fleet` lists all eight).

SCRIPT 4 (active-only counting, #424) — a `project_lead` `L4`, with a cap of 2 on the
role `worker` (set by the test: the shipped card holds no `set_limit`; script 2's
finished workers `WA` and `WB` are paused, so they hold no slot):
  1. `L4` spawns `W41` and `W42`; a third `spawn` is refused `limit` (scope role, 2/2);
  2. `W41` reports `done` (once L4 has read the refusal), which pauses it: the registry's
     `get_count worker` drops to 1 while `fleet` still lists it;
  3. `L4` (sent a second message) spawns `W43` (now 2/2 again) and `pause`s `W42`:
     `get_count worker` is 1;
  4. `L4` (a third message) spawns `W44` (2/2), `resume`s `W42`: refused `limit` while
     two others are active (the agent stays paused), `pause`s `W44`, then `resume`s
     `W42`: it resumes and `get_count worker` is 2 again.

SCRIPT 3 (from a fork point, #424) — a source agent `SRC` (a `vllm_backend` agent with
a memory store; the test gives it two finished turns, then `mark`s it) and a lead `L2`
(created by the test, holding `fork` on `SRC`):
  1. `spawn role=worker name=W2 from:{agent:SRC, at:<the mark's at>}` creates an agent;
  2. a second `spawn ... from.at=<the first row of a turn>` is refused `cuts_turn`
     (fork's own refusal passes through) and creates nothing.
  Checks (registry, records, history): the reply carries `from` and `copied`; W2 has
  role worker, the card's directive as its system prompt, `spawned_by` the registry and
  the SAME `memory_store_id` as SRC; it sits beside SRC (same parent); it holds SRC's
  rows up to `at` with the same ids and none after; the refused spawn left no agent;
  `fleet` lists W2 under its role. NOT part of any CI run: coded, run by hand.
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
AR = "agentic"
MODEL = "mock-scripted"
KEY = re.compile(r"\b(L0|L1|L2|L4|SA|SB|SRC|WA|WB|W41|W42|W43|W44):")
SRC, L2 = "src_agent", "lead_two"

WRITER = ("---\nname: writer\npurpose: Writes the first draft of a text.\ntier: fast\n"
          "grants:\n  $registry: [report]\n---\nYou write clearly and briefly.\n")
WORKER = ("---\nname: worker\npurpose: Does one small piece of the work and reports it.\n"
          "tier: fast\ngrants:\n  $registry: [report]\n---\nYou do one piece and report it.\n")


def text_of(m: dict) -> str:
    c = m.get("content")
    return c if isinstance(c, str) else json.dumps(c)


def call(verb: str, _to: str = AR, **args) -> str:
    """One `<call name="send">` to `_to` (the registry); a dict value rides as child tags."""
    def tag(k, v):
        if isinstance(v, dict):
            return f"<{k}>" + "".join(tag(a, b) for a, b in v.items()) + f"</{k}>"
        return f"<{k}>{v}</{k}>"
    return (f'<call name="send">\n<target_id>{_to}</target_id>\n<type>{verb}</type>\n'
            + "".join(tag(k, v) for k, v in args.items()) + "\n</call>")


DONE = 'Finished.\n<call name="done"></call>'


def main() -> int:
    name, _ = runtime(REPO, RUNTIME)
    spec = ensure_runtime(REPO, name)
    m = VllmMock(model=MODEL).start()
    work = Path(tempfile.mkdtemp(prefix="e2e-agentic-resources-"))
    roles_dir = work / "roles"
    roles_dir.mkdir()
    (roles_dir / "writer.md").write_text(WRITER)
    (roles_dir / "worker.md").write_text(WORKER)
    hk = HostKernel(spec, work, reach=[AR, SRC, L2])
    hk.env["FANTASTIC_AGENTIC_RESOURCES"] = "1"
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

    def fleet() -> dict[str, dict]:
        out = {}
        for role, rows in (hk.send(AR, {"type": "fleet"}).get("roles") or {}).items():
            for row in rows:
                out[row["id"]] = {**row, "role": role}
        return out

    def named(label: str) -> str | None:
        """The id of the spawned agent whose record `name` is `label`."""
        return next((i for i, r in records().items() if r.get("name") == label), None)

    # what each scripted agent reported: `report` stores nothing on the record, so the
    # probe notes it when the agent's script emits the report
    reported: dict[str, str] = {}
    finished: dict[str, threading.Event] = {}

    def result_of(label: str):
        return reported.get(label)

    def finish(label: str, result: str) -> str:
        """The scripted `report done` of agent `label`, noted for the waiters."""
        reported[label] = result
        finished.setdefault(label, threading.Event()).set()
        return call("report", state="done", result=result)

    def until(cond, bound: float = 90) -> bool:
        end = time.monotonic() + bound
        while time.monotonic() < end:
            if cond():
                return True
            time.sleep(0.1)
        return False

    def done_of(label: str) -> bool:
        return finished.setdefault(label, threading.Event()).is_set()

    # ── the scripts: one queue per agent key; an entry is a callable ─────────────
    seen: dict[str, list[str]] = {}          # key -> the last user turn of each request
    gate_sa = threading.Event()              # SA reached its third turn (its refusal was read)
    gate_l4 = threading.Event()              # L4 read its refusal and the count
    lock = threading.Lock()

    def waits_for(*labels: str):
        until(lambda: all(done_of(label) for label in labels))

    def sa_report() -> str:
        gate_sa.set()
        waits_for("WA")
        return finish("SA", f"part A done: {result_of('WA')}")

    def wa_report() -> str:
        gate_sa.wait(60)
        return finish("WA", "leaf a ok")

    def sb_report() -> str:
        waits_for("WB")
        return finish("SB", f"part B done: {result_of('WB')}")

    def l0_report() -> str:
        waits_for("SA", "SB")
        return finish("L0", f"ship it: {result_of('SA')} / {result_of('SB')}")

    scripts: dict[str, list] = {
        "L1": [call("roles"),
               call("spawn", role="writer"), call("spawn", role="writer"),
               call("spawn", role="writer"),
               call("get_count", role="writer"),
               call("report", state="done", result="two writers"), DONE],
        "L0": [call("spawn", role="project_lead", name="SA", limits={"total": 1},
                    task="SA: do part A of the work"),
               call("spawn", role="project_lead", name="SB", limits={"total": 1},
                    task="SB: do part B of the work"),
               l0_report, DONE],
        "SA": [call("spawn", role="worker", name="WA", task="WA: do leaf a"),
               call("spawn", role="worker", name="WX", task="WX: one too many"),
               sa_report, DONE],
        "SB": [call("spawn", role="worker", name="WB", task="WB: do leaf b"),
               sb_report, DONE],
        "WA": [wa_report, DONE],
        "WB": [lambda: finish("WB", "leaf b ok"), DONE],
        "L4": [call("spawn", role="worker", name="W41", task="W41: do piece one"),
               call("spawn", role="worker", name="W42", task="W42: do piece two"),
               call("spawn", role="worker", name="W43", task="W43: one too many"),
               lambda: (gate_l4.set(), call("get_count", role="worker"))[1],
               DONE],
        "W41": [lambda: (gate_l4.wait(60), finish("W41", "piece one ok"))[1], DONE],
        "W42": [DONE],
        "W43": [DONE],
        "W44": [DONE],
        "SRC": [],
        "L2": [lambda: call("spawn", role="worker", name="W2",
                            **{"from": {"agent": SRC, "at": mark_at["at"]}}),
               lambda: call("spawn", role="worker", name="W3",
                            **{"from": {"agent": SRC, "at": mark_at["cut"]}}),
               DONE],
    }
    mark_at: dict = {}

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
        # a finished agent is paused by its `done` report, which may cut its turn
        # before the trailing DONE request: only DONE entries may be left
        return all(all(e == DONE for e in scripts[k]) for k in keys)

    def idle(bound: float = 90) -> bool:
        end = time.monotonic() + bound
        while time.monotonic() < end:
            ids = set(fleet())
            tasks = hk.send("kernel", {"type": "ps"}).get("tasks") or []
            if not any(t.get("agent") in ids for t in tasks):
                return True
            time.sleep(0.1)
        return False

    tier = {"handler_module": "vllm_backend.tools", "endpoint": m.url, "model": MODEL,
            "max_tokens": 8192, "context_window": 262144, "kv_slots": 8,
            "kv_overflow": "evict", "memory_store_id": "memory"}
    try:
        rec = hk.create(parent_id="rest", handler_module="agentic_resources.tools", id=AR,
                        role_dirs=[str(roles_dir)], tiers={"fast": tier, "lead": tier},
                        callable_by={"$self": "*", "$parent": "*"})
        need(rec.get("id") == AR, f"create the registry: {str(rec)[:200]}")
        roles = hk.send(AR, {"type": "roles"})
        names = sorted(r["name"] for r in roles.get("roles") or [])
        need(names == ["project_lead", "worker", "writer"] and not roles.get("errors"),
             f"the registry lists its cards and the shipped project_lead: {roles}")

        # ── script 1 ─────────────────────────────────────────────────────────
        need(hk.send(AR, {"type": "set_limit", "role": "writer", "max": 2}).get("ok") is True,
             "set_limit role=writer max=2")
        out = hk.send(AR, {"type": "spawn", "role": "project_lead",
                           "task": "L1: list the roles, then take two writers"})
        need(bool(out.get("id")), f"the test spawns the lead L1: {str(out)[:200]}")
        need(until(lambda: spent("L1")) and idle(), "L1's script runs to its end and settles")
        u = seen.get("L1", []) + [""] * 7
        need("writer" in u[1] and "Writes the first draft" in u[1] and "project_lead" in u[1],
             f"1 · the `roles` reply lists writer and project_lead: {u[1][:160]!r}")
        need("<id>" in u[2] and "<role>writer</role>" in u[2] and "<id>" in u[3],
             "2 · both spawns create an agent")
        need("<error>limit</error>" in u[4] and "<scope>role</scope>" in u[4],
             f"3 · the third spawn is refused at the limit: {u[4][:200]!r}")
        need("<running>2</running>" in u[5] and "<max>2</max>" in u[5], f"4 · get_count says 2 of 2: {u[5][:160]!r}")
        need(hk.send(AR, {"type": "get_count", "role": "writer"}) == {"running": 2, "max": 2},
             "the registry agrees: get_count writer = 2/2")
        writers = [i for i, r in fleet().items() if r["role"] == "writer"]
        need(len(writers) == 2, f"fleet lists exactly two writers: {writers}")

        # ── script 2 ─────────────────────────────────────────────────────────
        out = hk.send(AR, {"type": "spawn", "role": "project_lead", "name": "L0",
                           "task": "L0: ship it in two parts"})
        need(bool(out.get("id")), f"the test spawns the top lead L0: {str(out)[:200]}")
        need(until(lambda: spent("L0", "SA", "SB", "WA", "WB"), 120) and idle(),
             "every script runs to its end and settles")
        need(all(done_of(n) for n in ("L0", "SA", "SB", "WA", "WB")),
             "every scripted agent reported done")
        sa = (seen.get("SA") or []) + [""] * 4
        need("<error>limit</error>" in sa[2] and "subtree_total" in sa[2],
             f"SA cannot spawn past its quota of 1: {sa[2][:220]!r}")
        recs = records()
        quotas = {n: (recs.get(named(n)) or {}).get("limits") for n in ("SA", "SB")}
        need(quotas == {"SA": {"total": 1}, "SB": {"total": 1}},
             f"the sub-leads hold their split quotas: {quotas}")
        need(named("WX") is None, "the refused spawn made no agent")
        top = result_of("L0") or ""
        need("part A done: leaf a ok" in top and "part B done: leaf b ok" in top,
             f"the top report condenses both parts: {top!r}")
        # the reports reach the top of the tree: parents are the callers
        parents = {n: (recs.get(named(n)) or {}).get("parent_id") for n in ("SA", "WA", "WB")}
        need(parents["SA"] == named("L0") and parents["WA"] == named("SA")
             and parents["WB"] == named("SB"), f"the tree follows the callers: {parents}")

        # the agent tree, from the root's reflect: nested under the callers
        tree = hk.send(root, {"type": "reflect", "tree": "all"})

        def find(node, aid):
            if isinstance(node, dict):
                if node.get("id") == aid:
                    return node
                for v in node.values():
                    got = find(v, aid)
                    if got is not None:
                        return got
            elif isinstance(node, list):
                for v in node:
                    got = find(v, aid)
                    if got is not None:
                        return got
            return None

        l0 = find(tree, named("L0"))
        need(l0 is not None and find(l0, named("WA")) is not None,
             "reflect tree=all nests WA under L0")
        need(all("task" not in (recs.get(named(n)) or {}) for n in ("L0", "SA", "WA")),
             "no record carries a task line: report stores nothing")

        # nothing is cleaned up; a done report pauses the reporter and frees its slot
        every = set(fleet())
        need({named(n) for n in ("L0", "SA", "SB", "WA", "WB")} <= every and len(every) == 8,
             f"all eight spawned agents are still there: {len(every)}")
        # the pause on a report is detached: wait for the six reporters (L1, L0, SA, SB,
        # WA, WB) to be paused, which leaves only L1's two idle writers active
        need(until(lambda: hk.send(AR, {"type": "get_count"}).get("running") == 2),
             f"get_count counts only the active agents: {hk.send(AR, {'type': 'get_count'})}")
        need(hk.send(AR, {"type": "get_count"}) == {"running": 2, "max": None},
             "a done report freed its slot: six of eight are paused, two writers are active")
        need(all(a in records() for a in every), "no record was removed")

        # ── script 4: active-only counting ───────────────────────────────────
        def worker_count() -> int | None:
            return hk.send(AR, {"type": "get_count", "role": "worker"}).get("running")

        need(worker_count() == 0, "script 2's finished workers hold no slot")
        need(hk.send(AR, {"type": "set_limit", "role": "worker", "max": 2}).get("ok") is True,
             "set_limit role=worker max=2")
        out = hk.send(AR, {"type": "spawn", "role": "project_lead", "name": "L4",
                           "task": "L4: take two workers, then a third"})
        need(bool(out.get("id")), f"the test spawns the lead L4: {str(out)[:200]}")
        need(until(lambda: spent("L4", "W41")) and idle(), "L4 and W41 run to their end")
        u4 = (seen.get("L4") or []) + [""] * 5
        need("<id>" in u4[1] and "<id>" in u4[2], "1 · L4's two spawns create an agent")
        need("<error>limit</error>" in u4[3] and "<scope>role</scope>" in u4[3]
             and "<running>2</running>" in u4[3],
             f"1 · the third spawn is refused at the limit of 2: {u4[3][:200]!r}")
        need(until(lambda: worker_count() == 1),
             f"2 · W41's done report paused it and freed its slot: {worker_count()}")
        need(named("W41") in fleet(), "2 · fleet still lists the finished W41")
        l4 = named("L4")
        hk.send(root, {"type": "update_agent", "id": l4,
                       "callable_by": {**dict((records().get(l4) or {}).get("callable_by") or {}),
                                       "rest": "*"}})
        scripts["L4"] += [call("spawn", role="worker", name="W43", task="W43: take the slot"),
                          lambda: call("pause", _to=named("W42")), DONE]
        n4 = len(seen.get("L4") or [])
        out = hk.send(l4, {"type": "send", "text": "L4: spawn W43, pause W42"})
        need(not out.get("error"), f"L4 runs again: {str(out)[:120]}")
        need(until(lambda: spent("L4")) and idle(), "L4's second script runs to its end")
        p2 = (seen.get("L4") or [])[n4:] + [""] * 3
        need("<id>" in p2[1], f"3 · the spawn succeeds once a slot is free: {p2[1][:160]!r}")
        need("<paused>true</paused>" in p2[2], f"3 · the lead pauses W42: {p2[2][:160]!r}")
        need(hk.send(AR, {"type": "get_count", "role": "worker"}) == {"running": 1, "max": 2},
             "3 · pausing W42 freed its slot: 1 of 2")
        scripts["L4"] += [call("spawn", role="worker", name="W44", task="W44: fill the slot"),
                          lambda: call("resume", _to=named("W42")),
                          lambda: call("pause", _to=named("W44")),
                          lambda: call("resume", _to=named("W42")), DONE]
        n4 = len(seen.get("L4") or [])
        out = hk.send(l4, {"type": "send", "text": "L4: fill, resume W42, free, resume"})
        need(not out.get("error"), f"L4 runs a third time: {str(out)[:120]}")
        need(until(lambda: spent("L4")) and idle(), "L4's third script runs to its end")
        p3 = (seen.get("L4") or [])[n4:] + [""] * 5
        need("<id>" in p3[1], f"4 · the spawn fills the free slot: {p3[1][:160]!r}")
        need("<error>limit</error>" in p3[2],
             f"4 · resuming W42 is refused while two others are active: {p3[2][:200]!r}")
        need("<resumed>true</resumed>" in p3[4],
             f"4 · resuming W42 succeeds after a pause: {p3[4][:160]!r}")
        need(until(lambda: worker_count() == 2),
             f"4 · the resumed W42 holds a slot again: {worker_count()}")
        hk.send(AR, {"type": "set_limit", "role": "worker", "max": None})

        # ── script 3: spawn from a fork point ────────────────────────────────
        def cb_of(aid: str) -> dict:
            return dict((records().get(aid) or {}).get("callable_by") or {})

        def settled(*ids: str, bound: float = 90) -> bool:
            end = time.monotonic() + bound
            while time.monotonic() < end:
                tasks = hk.send("kernel", {"type": "ps"}).get("tasks") or []
                if not any(t.get("agent") in ids for t in tasks):
                    return True
                time.sleep(0.1)
            return False

        for who in ("memory",):
            hk.send(root, {"type": "update_agent", "id": who,
                           "callable_by": {**cb_of(who), SRC: "*", L2: "*"}})
        hk.send(root, {"type": "update_agent", "id": AR,
                       "callable_by": {**cb_of(AR), L2: "*"}})
        common = {k: v for k, v in tier.items() if k != "handler_module"}
        rec = hk.create(parent_id="rest", handler_module="vllm_backend.tools", id=SRC,
                        **common, can_call={"$self": "*", "memory": "*"},
                        callable_by={"$self": "*", "$parent": "*", "rest": "*"})
        need(rec.get("id") == SRC, f"create the source agent: {str(rec)[:200]}")
        rec = hk.create(parent_id="rest", handler_module="vllm_backend.tools", id=L2,
                        **common, can_call={"$self": "*", "memory": "*", AR: "*",
                                            SRC: ["fork"]},
                        callable_by={"$self": "*", "$parent": "*", "rest": "*"})
        need(rec.get("id") == L2, f"create the lead L2: {str(rec)[:200]}")
        for n in (1, 2):
            out = hk.send(SRC, {"type": "send", "text": f"SRC: step {n}"})
            need(not out.get("error"), f"SRC takes turn {n}: {str(out)[:120]}")
        need(settled(SRC), "SRC is idle after its two turns")
        marked = hk.send(SRC, {"type": "mark", "label": "two steps done"})
        at = marked.get("at")
        need(isinstance(at, int), f"SRC marks its history: {marked}")
        mark_at["at"] = at
        hist = hk.send(SRC, {"type": "history"}).get("rows") or []
        first_user = next((r["id"] for r in hist if r.get("role") == "user"), None)
        need(first_user is not None and first_user <= at, f"a turn's first row exists: {hist[:2]}")
        mark_at["cut"] = first_user
        want_ids = [r["id"] for r in hist if r.get("id") is not None and r["id"] <= at]
        before = set(records())

        out = hk.send(L2, {"type": "send", "text": "L2: branch a worker from SRC's mark"})
        need(not out.get("error"), f"L2 runs: {str(out)[:120]}")
        need(until(lambda: spent("L2")) and settled(L2, SRC), "L2's script runs to its end")
        l2 = (seen.get("L2") or []) + [""] * 3
        need("<id>" in l2[1] and "<role>worker</role>" in l2[1] and "<copied>" in l2[1]
             and "<from>" in l2[1], f"1 · the spawn from the mark answers: {l2[1][:240]!r}")
        need("<reason>cuts_turn</reason>" in l2[2],
             f"2 · an `at` inside a turn is refused cuts_turn: {l2[2][:240]!r}")
        w2 = named("W2")
        need(w2 is not None and named("W3") is None, "W2 exists; the refused spawn made nothing")
        need(set(records()) - before == {w2},
             f"nothing else appeared: {sorted(set(records()) - before)}")
        recs = records()
        w, s_rec = recs.get(w2) or {}, recs.get(SRC) or {}
        need(w.get("role") == "worker" and w.get("spawned_by") == AR
             and w.get("system_prompt") == "You do one piece and report it.",
             f"W2 carries the role card: {str(w)[:300]}")
        need(w.get("memory_store_id") == s_rec.get("memory_store_id") == "memory",
             "W2 keeps SRC's memory store")
        need(w.get("parent_id") == s_rec.get("parent_id"),
             "W2 sits beside SRC (fork's placement)")
        need(w2 in fleet() and fleet()[w2]["role"] == "worker", "fleet lists W2 under worker")
        hk.send(root, {"type": "update_agent", "id": w2,
                       "callable_by": {**cb_of(w2), "rest": "*"}})
        got = [r["id"] for r in hk.send(w2, {"type": "history"}).get("rows") or []
               if r.get("id") is not None]
        need(got == want_ids and len(got) > 0,
             f"W2 holds SRC's rows up to the mark, same ids: {got} vs {want_ids}")
    finally:
        hk.close()
        m.stop()
        shutil.rmtree(work, ignore_errors=True)

    verdict(not bad, f"{RUNTIME}: {len(bad)} failed" + (f" — first: {bad[0]}" if bad else ""))
    return 0 if not bad else 1


if __name__ == "__main__":
    sys.exit(main())
