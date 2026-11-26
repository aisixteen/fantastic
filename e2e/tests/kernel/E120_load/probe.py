"""kernel/load — hydration and batch throughput at scale. NO MODEL.

On the PACKAGED rust host, every call through `web_rest`:

  1. BASELINE: the driver's seed alone boots (spawn → serving) in `t0`; `rest`'s
     root reflect carries no runtime frame (operator only), the operator's
     one-shot `kernel reflect` after the stop names the runtime;
  2. HYDRATE: the same seed plus N=1000 agent records on disk (`ram_store`
     children of the `rest` leg, no grant field) boots in `t1`; every one of them
     is listed by `rest`'s paged `reflect tree=ids` (its own tree) and indexed by
     the root's; a sample answers `reflect`;
  3. REST: 500 `reflect` POSTs over 16 client threads, every one answered;
  4. ONE BATCH: an AI agent (`vllm_backend` against a scripted local server),
     holding one `reflect` line per target, answers one turn with 500 `<call>`s — ONE kernel batch of 500 items to 500
     hydrated agents; each reply is its own message (`k of 500 in batch …`, #795),
     read by the passes that follow, coalesced (a reply landing while a pass works
     waits for the ONE next pass): every reply read once, by few passes. Timed from
     the `send` POST until the agent is quiet with every reply read (the scripted
     answers' own streams are a few ms of it).

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

import subprocess

import yaml

E2E = next(d for d in Path(__file__).resolve().parents if (d / ".e2e-root").exists())
sys.path.insert(0, str(E2E))

from helpers.host.driver import HostKernel, ensure_runtime, runtime
from helpers.lines import ALL, PROVIDER, call
from helpers.verdict import verdict
from mocks.vllm import VllmMock

REPO = E2E.parent
RUNTIME = os.environ.get("RUNTIME", "")
N_AGENTS = 1000
N_BATCH = 500
N_REST = 500
AI = "ai"
PROV = f"{AI}_prov"  # the provider the chat completes through
MODEL = "mock-load"
#: Seconds. Generous on purpose: only a gross regression fails.
BOUNDS = {"boot_1000": 90.0, "reflect_ids": 15.0, "rest_500": 60.0, "batch_500": 90.0}


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
        }, sort_keys=False))


def ids_of(hk: HostKernel, target: str) -> tuple[list[str], int]:
    """Every id of `target`'s `reflect tree=ids`, following `next`; (ids, pages)."""
    out: list[str] = []
    after = None
    pages = 0
    while True:
        p = {"type": "reflect", "tree": "ids"}
        if after:
            p["after"] = after
        r = hk.send(target, p, timeout=120)
        pages += 1
        tree = r.get("tree") if isinstance(r, dict) else None
        if not isinstance(tree, list):
            return out, pages
        out += [x if isinstance(x, str) else x.get("id") for x in tree]
        after = r.get("next")
        if not after:
            return out, pages


def timed_boot(spec: dict, work: Path) -> tuple[HostKernel, float]:
    hk = HostKernel(spec, work)
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
    hk0, nums["boot_seed"] = timed_boot(spec, w0)
    try:
        r = hk0.send(hk0.root_id, {"type": "reflect", "tree": "none"})
        need(r.get("id") == hk0.root_id and "runtime" not in r,
             f"baseline boots: {nums['boot_seed']:.2f}s; rest's root reflect carries no runtime "
             f"(operator only): {str(r)[:160]}")
        hk0.close()
        # the operator's one-shot, with no daemon up (it takes the lock)
        one = subprocess.run([*hk0.cmd, "kernel", "reflect"], cwd=w0, env=hk0.env,
                             capture_output=True, text=True, timeout=120, check=False)
        try:
            op = json.loads(one.stdout)
        except ValueError:
            op = {"raw": (one.stdout + one.stderr)[-300:]}
        need(one.returncode == 0 and op.get("runtime") == name,
             f"the operator's `kernel reflect` names the runtime {name}: exit {one.returncode}, "
             f"{str(op)[:200]}")
    finally:
        hk0.close()
        shutil.rmtree(w0, ignore_errors=True)

    m = VllmMock(model=MODEL).start()
    work = Path(tempfile.mkdtemp(prefix="e2e-load-"))
    t = time.monotonic()
    seed_pool(work)
    print(f"     wrote {N_AGENTS} records in {time.monotonic() - t:.2f}s", flush=True)
    hk, nums["boot_1000"] = timed_boot(spec, work)
    root = hk.root_id
    try:
        # 2 · hydration
        need(nums["boot_1000"] <= BOUNDS["boot_1000"],
             f"{N_AGENTS} agents hydrate and serve in {nums['boot_1000']:.2f}s "
             f"(baseline {nums['boot_seed']:.2f}s; bound {BOUNDS['boot_1000']:.0f}s)")
        t = time.monotonic()
        listed_ids, pages = ids_of(hk, "rest")
        nums["reflect_ids"] = time.monotonic() - t
        listed = set(listed_ids)
        missing = [pool_id(i) for i in range(N_AGENTS) if pool_id(i) not in listed]
        need(not missing and nums["reflect_ids"] <= BOUNDS["reflect_ids"],
             f"rest's reflect tree=ids lists all {N_AGENTS} ({len(listed)} ids, {pages} pages) in "
             f"{nums['reflect_ids']:.2f}s; missing {len(missing)} {missing[:5]}")
        root_ids, _ = ids_of(hk, "kernel")
        flat = set(root_ids)
        need(all(pool_id(i) in flat for i in range(N_AGENTS)),
             f"the root's reflect tree=ids indexes all {N_AGENTS} ({len(flat)} ids)")
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
        batch = [pool_id(i) for i in range(N_BATCH)]
        # a provider (the vllm_backend) and the chat (ai_chat.tools) naming it
        rec = hk.create(parent_id="rest", handler_module="vllm_backend.tools", id=PROV,
                        endpoint=m.url, model=MODEL,
                        context_window=262144, kv_slots=4, kv_overflow="evict",
                        memory_store_id="memory", can={"memory": ALL})
        need(rec.get("id") == PROV, f"create {PROV}: {str(rec)[:160]}")
        rec = hk.create(parent_id="rest", handler_module="ai_chat.tools", id=AI,
                        provider_id=PROV, model=MODEL, max_tokens=8192,
                        context_window=262144, memory_store_id="memory",
                        can={"memory": ALL, PROV: PROVIDER,
                             **{p: call("reflect") for p in batch}})
        need(rec.get("id") == AI, f"create {AI}: {str(rec)[:160]}")
        calls = "\n".join(f'<call name="send"><routing><target_id>{p}</target_id></routing><payload><type>reflect</type>'
                          f'<tree>none</tree></payload></call>' for p in batch)
        m.reply("Reflecting the pool.\n" + calls, 'Seen them all.\n<call name="done"></call>')
        t_sent = time.monotonic()
        out = hk.send(AI, {"type": "send", "text": "reflect the pool"}, timeout=300)
        need(out.get("accepted") is True, f"the send is acked: {str(out)[:160]}")

        def read_replies(t: dict) -> list[str]:
            """The reply blocks a request carries, each user message once."""
            got = []
            for x in t["body"].get("messages") or []:
                if x.get("role") != "user":
                    continue
                c = x.get("content")
                c = c if isinstance(c, str) else json.dumps(c)
                got += re.findall(r"\[reply to \S+ · (p\d{4}) reflect[^\]]*of 500 in batch", c)
            return got

        def quiet() -> bool:
            return hk.quiet(AI)

        end = t_sent + BOUNDS["batch_500"] * 2
        while time.monotonic() < end and not (
                len(m.turns()) >= 2 and sorted(set(read_replies(m.turns()[-1]))) == batch and quiet()):
            time.sleep(0.02)
        nums["batch_500"] = time.monotonic() - t_sent
        turns = m.turns()
        last = read_replies(turns[-1]) if len(turns) >= 2 else []
        need(len(turns) >= 2 and nums["batch_500"] <= BOUNDS["batch_500"],
             f"one batch of {N_BATCH} dispatched and its replies read in "
             f"{nums['batch_500']:.2f}s = {N_BATCH / max(nums['batch_500'], 1e-9):.0f} items/s")
        need(sorted(last) == batch,
             f"every one of the {N_BATCH} replies is read, once each: {len(last)} read, "
             f"{len(set(last))} distinct")
        # REPORTED: how many passes read the replies (a reply landing while a pass works
        # waits for the one next pass); the bound only catches one pass per reply
        passes = len(turns) - 1
        nums["batch_passes"] = passes
        need(1 <= passes <= N_BATCH // 4,
             f"the replies are read by coalesced passes, not one pass per reply: {passes}")
    finally:
        hk.close()
        m.stop()
        shutil.rmtree(work, ignore_errors=True)

    print(f"  NUMBERS {name}: " + json.dumps({k: round(v, 3) for k, v in nums.items()}), flush=True)
    verdict(not bad, f"{RUNTIME}: {len(bad)} failed" + (f" — first: {bad[0]}" if bad else ""))
    return 0 if not bad else 1


if __name__ == "__main__":
    sys.exit(main())
