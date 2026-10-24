"""kernel/load — hydration and batch throughput at scale. NO MODEL. Run SOLO.

On the PACKAGED host of `RUNTIME` (python | rust), every call through
`web_rest`:

  1. BASELINE: the driver's seed alone boots (spawn → serving) in `t0`;
  2. HYDRATE: the same seed plus N=1000 agent records on disk (`ram_store`
     children of the `rest` leg) boots in `t1`; every one of them is listed by
     `list_agents` and indexed by `reflect tree=ids`; a sample answers `reflect`;
  3. REST: 500 `reflect` POSTs over 16 client threads, every one answered;
  4. ONE BATCH: an AI agent (`vllm_backend` against a scripted local server)
     answers one turn with 500 `<call>`s — ONE kernel batch of 500 items to 500
     hydrated agents; the next request carries all 500 replies as one turn
     (`N of 500 in batch …`). Timed from the `send` POST to that next request
     (the scripted answer's own stream is a few ms of it).

The numbers are REPORTED; the bounds only catch a gross regression (`BOUNDS`).
"""

from __future__ import annotations

import json
import os
import re
import shutil
import sys
import tempfile
import time
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path

import yaml

E2E = next(d for d in Path(__file__).resolve().parents if (d / ".e2e-root").exists())
sys.path.insert(0, str(E2E))

from helpers.host.driver import HostKernel, ensure_runtime, runtime
from helpers.verdict import verdict
from mocks.vllm import VllmMock

REPO = E2E.parent
RUNTIME = os.environ.get("RUNTIME", "")
N_AGENTS = 1000
N_BATCH = 500
N_REST = 500
AI = "ai"
MODEL = "mock-load"
#: Seconds. Generous on purpose: only a gross regression fails.
BOUNDS = {"boot_1000": 90.0, "list_agents": 15.0, "rest_500": 60.0, "batch_500": 90.0}


def pool_id(i: int) -> str:
    return f"p{i:04d}"


def seed_pool(work: Path) -> None:
    """N records on disk, children of the `rest` leg (the test's operator), before
    the kernel starts: the loader hydrates them at boot."""
    base = work / ".fantastic/agents/web/agents/rest/agents"
    for i in range(N_AGENTS):
        d = base / pool_id(i)
        d.mkdir(parents=True, exist_ok=True)
        (d / "agent.yaml").write_text(yaml.safe_dump({
            "id": pool_id(i), "handler_module": "ram_store.tools", "parent_id": "rest",
            "callable_by": {"$self": "*", "$parent": "*", AI: ["reflect"]},
        }, sort_keys=False))


def timed_boot(spec: dict, work: Path, reach: list[str]) -> tuple[HostKernel, float]:
    hk = HostKernel(spec, work, reach=reach)
    t = time.monotonic()
    hk.boot()
    return hk, time.monotonic() - t


def main() -> int:
    name, _ = runtime(REPO, RUNTIME)
    spec = ensure_runtime(REPO, name)
    bad: list[str] = []
    nums: dict[str, float] = {}

    def need(cond: bool, what: str) -> None:
        print(f"  {'ok ' if cond else 'BAD'} {what}", flush=True)
        if not cond:
            bad.append(what)

    # 1 · baseline
    w0 = Path(tempfile.mkdtemp(prefix="e2e-load-base-"))
    hk0, nums["boot_seed"] = timed_boot(spec, w0, [])
    try:
        r = hk0.send(hk0.root_id, {"type": "reflect", "tree": "none"})
        need(r.get("runtime") == name, f"baseline boots: {nums['boot_seed']:.2f}s")
    finally:
        hk0.close()
        shutil.rmtree(w0, ignore_errors=True)

    m = VllmMock(model=MODEL).start()
    work = Path(tempfile.mkdtemp(prefix="e2e-load-"))
    t = time.monotonic()
    seed_pool(work)
    print(f"     wrote {N_AGENTS} records in {time.monotonic() - t:.2f}s", flush=True)
    hk, nums["boot_1000"] = timed_boot(spec, work, [AI])
    root = hk.root_id
    try:
        # 2 · hydration
        need(nums["boot_1000"] <= BOUNDS["boot_1000"],
             f"{N_AGENTS} agents hydrate and serve in {nums['boot_1000']:.2f}s "
             f"(baseline {nums['boot_seed']:.2f}s; bound {BOUNDS['boot_1000']:.0f}s)")
        t = time.monotonic()
        la = hk.send("kernel", {"type": "list_agents"}, timeout=120)
        nums["list_agents"] = time.monotonic() - t
        listed = {a.get("id") for a in la.get("agents") or []}
        missing = [pool_id(i) for i in range(N_AGENTS) if pool_id(i) not in listed]
        need(not missing and nums["list_agents"] <= BOUNDS["list_agents"],
             f"list_agents lists all {N_AGENTS} ({len(listed)} records) in "
             f"{nums['list_agents']:.2f}s; missing {len(missing)} {missing[:5]}")
        ids = hk.send(root, {"type": "reflect", "tree": "ids"}, timeout=120)
        flat = json.dumps(ids.get("tree"))
        need(all(f'"{pool_id(i)}"' in flat for i in range(N_AGENTS)),
             f"reflect tree=ids indexes all {N_AGENTS}")
        sample = [pool_id(i) for i in (0, N_AGENTS // 2, N_AGENTS - 1)]
        refl = {p: hk.send(p, {"type": "reflect"}).get("id") for p in sample}
        need(all(refl[p] == p for p in sample), f"a hydrated sample answers reflect: {refl}")

        # 3 · REST throughput
        targets = [pool_id(i) for i in range(N_REST)]
        t = time.monotonic()
        with ThreadPoolExecutor(16) as ex:
            got = list(ex.map(lambda p: hk.send(p, {"type": "reflect", "tree": "none"},
                                                timeout=120).get("id"), targets))
        nums["rest_500"] = time.monotonic() - t
        wrong = [(p, g) for p, g in zip(targets, got) if g != p]
        need(not wrong and nums["rest_500"] <= BOUNDS["rest_500"],
             f"{N_REST} REST reflects (16 threads) in {nums['rest_500']:.2f}s = "
             f"{N_REST / max(nums['rest_500'], 1e-9):.0f}/s; wrong {len(wrong)} {wrong[:3]}")

        # 4 · one batch of 500 from an AI turn
        r = hk.send(root, {"type": "update_agent", "id": "memory", "callable_by": {
            **next(a for a in la["agents"] if a.get("id") == "memory").get("callable_by", {}),
            AI: "*"}})
        need(r.get("updated") is True, f"memory accepts the AI: {str(r)[:160]}")
        batch = [pool_id(i) for i in range(N_BATCH)]
        rec = hk.create(parent_id="rest", handler_module="vllm_backend.tools", id=AI,
                        endpoint=m.url, model=MODEL, max_tokens=8192,
                        context_window=262144, kv_slots=4, kv_overflow="evict",
                        memory_store_id="memory",
                        can_call={"$self": "*", "memory": "*", **{p: ["reflect"] for p in batch}},
                        callable_by={"$self": "*", "$parent": "*", "rest": "*"})
        need(rec.get("id") == AI, f"create {AI}: {str(rec)[:160]}")
        calls = "\n".join(f'<call name="send"><target_id>{p}</target_id><type>reflect</type>'
                          f'<tree>none</tree></call>' for p in batch)
        m.reply("Reflecting the pool.\n" + calls, 'Seen them all.\n<call name="done"></call>')
        t_sent = time.monotonic()
        out = hk.send(AI, {"type": "send", "text": "reflect the pool"}, timeout=300)
        need(not out.get("error"), f"the batch turn ran: {str(out.get('error') or '')[:160]}")
        end = t_sent + BOUNDS["batch_500"] * 2
        while time.monotonic() < end and len(m.turns()) < 2:
            time.sleep(0.02)
        nums["batch_500"] = time.monotonic() - t_sent
        turns = m.turns()
        nxt = ""
        if len(turns) >= 2:
            msgs = turns[1]["body"].get("messages") or []
            nxt = next((x.get("content") for x in reversed(msgs) if x.get("role") == "user"), "")
            nxt = nxt if isinstance(nxt, str) else json.dumps(nxt)
        blocks = re.findall(r"\[reply to \S+ · (p\d{4}) reflect[^\]]*of 500 in batch", nxt)
        need(len(turns) >= 2 and nums["batch_500"] <= BOUNDS["batch_500"],
             f"one batch of {N_BATCH} dispatched and its replies delivered in "
             f"{nums['batch_500']:.2f}s = {N_BATCH / max(nums['batch_500'], 1e-9):.0f} items/s")
        need(sorted(blocks) == batch,
             f"the next request carries all {N_BATCH} replies as one turn: {len(blocks)} blocks, "
             f"{len(nxt)} chars")
        end = time.monotonic() + 60
        while time.monotonic() < end and any(
                x.get("agent") == AI for x in hk.send("kernel", {"type": "ps"}).get("tasks") or []):
            time.sleep(0.05)
    finally:
        hk.close()
        m.stop()
        shutil.rmtree(work, ignore_errors=True)

    print(f"  NUMBERS {name}: " + json.dumps({k: round(v, 3) for k, v in nums.items()}), flush=True)
    verdict(not bad, f"{RUNTIME}: {len(bad)} failed" + (f" — first: {bad[0]}" if bad else ""))
    return 0 if not bad else 1


if __name__ == "__main__":
    sys.exit(main())
