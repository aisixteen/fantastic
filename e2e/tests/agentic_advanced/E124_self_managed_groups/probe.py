"""LIVE TEST — SELF-MANAGED GROUPS (#174): ONE master AI finishes a queue of Exercism
python tasks by running up to `MAX_GROUPS` worker groups at once, from its prompt
alone. There is no group code: a group is three agents the master creates under one
id prefix (`g<k>_worker` an AI, `g<k>_editor` a file_bridge, `g<k>_runner` an
exec_bridge — E43's runner trio) with the kernel verbs every AI has, and deletes when
its task is checked.

THE HARNESS CREATES NO GROUPS. It builds the queue (`queue`, a file agent over the
copied exercises), opens the memory and key doors for the worker ids the master may
use, creates the master with its recipe as `pre_system_prompt`
(`helpers.groups.master_prompt`), says "begin", and then only watches: it samples the
live agent ids on a timer into a `GroupLedger`.

Scored on:
  1. the bound held — never more than MAX_GROUPS groups live in one sample;
  2. no orphans — no group agent is left when the master says it is done;
  3. every task's own suite passes, rerun by the probe, test files untouched;
  4. the master's prompt stayed bounded — its largest turn input (system prompt plus
     conversation, in characters) is under MASTER_INPUT_CHARS_MAX.

Reported: the model per role (`BK.roles_used()`), wall time, each group's lifetime,
turns per agent, and — when the run is metered (E2E_METER=1) — tokens, cache hits
and cost from the meter. Run it twice, MAX_GROUPS=1 and MAX_GROUPS=3, to compare.
"""

from __future__ import annotations

import asyncio
import json
import os
import subprocess
import sys
import time
from pathlib import Path

sys.path.insert(
    0,
    str(
        next(d for d in Path(__file__).resolve().parents if (d / ".e2e-root").exists())
    ),
)
from backends import backend as _resolve_backend  # noqa: E402
from helpers import Limits, connect, creds, memory, verdict  # noqa: E402
from helpers.ai import IDLE_END, QuietWatch  # noqa: E402
from helpers.groups import PRACTICE, GroupLedger, group_of, make_queue, master_prompt, wall_seconds  # noqa: E402
from helpers.kernel import home, open_kernel  # noqa: E402

BK = _resolve_backend()
W = 78
MASTER = "ai"
QUEUE = "queue"
PREFIX = "g"


def _required_int(var: str) -> int:
    v = os.environ.get(var)
    if not v:
        raise SystemExit(f"{var} is unset. It is the run's to name, never guessed (e.g. {var}=1, then {var}=3).")
    return int(v)


MAX_GROUPS = _required_int("MAX_GROUPS")
EXERCISES = [e.strip() for e in (os.environ.get("EXERCISES") or "").split(",") if e.strip()]
if not EXERCISES:
    raise SystemExit("EXERCISES is unset: a comma list of polyglot python exercises (test.yaml names one).")
QUEUE_SIZE = int(os.environ.get("QUEUE_SIZE") or len(EXERCISES))
TASKS = EXERCISES[:QUEUE_SIZE]
MASTER_INPUT_CHARS_MAX = _required_int("MASTER_INPUT_CHARS_MAX")
CTX = int(os.environ.get("CTX", "65536"))
UNATTENDED = float(os.environ.get("UNATTENDED", "3600"))
SAMPLE_EVERY = float(os.environ.get("SAMPLE_EVERY", "5"))
TEST_CMD = [sys.executable, "-m", "pytest", "-q"]
#: the meter's tag for this test: run.py names a test `<group>.<id>`
METER_TAG = "agentic_advanced.self_managed_groups"


def worker_ids() -> list[str]:
    # one new group per task, never reused: the master can use at most this many
    return [f"{PREFIX}{k}_worker" for k in range(1, len(TASKS) + 1)]


def worker_record() -> dict:
    """The fields every worker gets verbatim: the plug's (model aside — the master
    sets the worker's model), the memory store and the window, and the doors the
    worker needs whatever its group (memory, the key store)."""
    rec = {k: v for k, v in BK.record(role="worker").items() if k != "model"}
    base = {"$parent": "*", **({creds.CREDS: creds.GET} if BK.key_ref else {})}
    return {**rec, "context_window": CTX, **memory.ai_memory_meta(base)}


def run_suite(cwd: str) -> tuple[bool, str]:
    try:
        r = subprocess.run(TEST_CMD, cwd=cwd, capture_output=True, text=True, check=False, timeout=300,
                           env={**os.environ, "PYTHONDONTWRITEBYTECODE": "1"})
    except subprocess.TimeoutExpired:
        return False, "suite timed out (300s)"
    out = (r.stdout + r.stderr).strip().splitlines()
    return r.returncode == 0, next((ln for ln in reversed(out) if ln.strip()), "")[:80]


async def live_ids(root) -> set[str]:
    r = (await root.send([{"target": "kernel", "payload": {"type": "list_agents"}}]))[0]["result"]
    return {a["id"] for a in (r or {}).get("agents") or []}


async def rows(root, aid) -> list[dict]:
    try:
        return await memory.rows_of(root, aid, conversation_only=True)
    except Exception:  # a deleted agent's rows may be gone with it
        return []


def max_turn_input(prompt: str, conv: list[dict]) -> int:
    """The largest input the master sent in one turn, in characters: its system
    prompt plus every conversation row before that turn's reply."""
    best, seen = 0, len(prompt)
    for r in conv:
        if r.get("role") == "assistant":
            best = max(best, seen)
        seen += len(str(r.get("content") or ""))
    return best


def meter_rows() -> list[dict]:
    run = os.environ.get("E2E_RUN_DIR")
    path = Path(run) / "meter.jsonl" if run else None
    if not path or not path.exists():
        return []
    out = []
    for line in path.read_text().splitlines():
        r = json.loads(line)
        if r.get("tag") == METER_TAG:
            out.append(r)
    return out


async def main() -> None:
    if not all((PRACTICE / name).is_dir() for name in TASKS):
        missing = [n for n in TASKS if not (PRACTICE / n).is_dir()]
        raise SystemExit(f"exercises missing under {PRACTICE}: {missing} — clone the polyglot benchmark there (see tests/coding_exercism/README.md)")
    wmodel = BK.model_for("worker")
    BK.model_for("master")
    print("=" * (W + 4))
    print(f"  SELF-MANAGED GROUPS · {BK.name} · MAX_GROUPS={MAX_GROUPS} · tasks={TASKS}")
    for role, model in BK.roles_used().items():
        print(f"    role {role:<8} -> {model}")
    print("=" * (W + 4))
    root, core, close = await open_kernel("groups_", reach=[QUEUE, MASTER, "cli"])
    try:
        workroot = Path(os.getcwd())
        tasks = make_queue(TASKS, workroot)
        queue_dir = str(workroot / "queue")
        protected = {t["name"]: {f: (Path(t["dir"]) / f).read_text() for f in t["tests"]} for t in tasks}
        made = (await root.send([{"target": root.id, "payload": {
            "type": "create_agent", "parent_id": home(root), "id": QUEUE,
            "handler_module": "file_bridge.tools", "allow": ["."], "root": queue_dir,
            "ingress_rule": "allow_all_ingress",
            "callable_by": {MASTER: "*", "rest": "*", "web": "*"},
            "description": "The task queue: one folder per task, README.task.md in each.",
        }}]))[0]["result"]
        assert not (made or {}).get("error"), made
        # THE DOORS for the workers the master may create — the stores' halves only
        # (each worker's own `can_call` is the master's to write, from its prompt).
        for wid in worker_ids():
            await memory.open_ingress(root, memory.MEMORY, wid)
            if BK.key_ref:
                await creds.open_store(root, wid)

        prompt = master_prompt(MAX_GROUPS, wmodel, queue_dir, PREFIX,
                               worker_record=worker_record(), test_command=TEST_CMD)
        extra = {"caching": "provider_cache"} if BK.name == "anthropic" else {}
        conn = await connect(
            root, core, BK,
            can_call={"kernel": ["create_agent", "delete_agent"], QUEUE: "*", "cli": "*"},
            record=BK.record(role="master", pre_system_prompt=prompt, **extra),
            limits=Limits.from_env(os.environ),
        )
        ledger = GroupLedger(PREFIX)
        t0 = time.monotonic()

        async def sample() -> None:
            while True:
                ledger.observe(time.monotonic() - t0, await live_ids(root))
                await asyncio.sleep(SAMPLE_EVERY)

        sampler = asyncio.create_task(sample())
        kick = asyncio.create_task(root.send([{"target": MASTER, "timeout": UNATTENDED, "payload": {
            "type": "send", "client_id": "groups",
            "text": f"Begin. The queue has {len(tasks)} tasks. Work through all of them."}}]))
        print(f"  kicked off the master; watching up to {UNATTENDED:.0f}s\n")
        watch = QuietWatch(root)
        finished = False
        seen = 0
        while time.monotonic() - t0 < UNATTENDED:
            await asyncio.sleep(15.0)
            conv = await rows(root, MASTER)
            for r in conv[seen:]:
                body = " ".join(str(r.get("content") or "").split())
                print(f"      master {r.get('role') or r.get('kind'):9} {body[:200]}", flush=True)
            seen = len(conv)
            live = ledger.samples[-1][1] if ledger.samples else frozenset()
            groups_live = sorted({g for g in (group_of(i, PREFIX) for i in live) if g is not None})
            print(f"  · {time.monotonic() - t0:5.0f}s live groups={groups_live} max={ledger.max_live()}", flush=True)
            said = " ".join(str(r.get("content") or "") for r in conv if r.get("role") == "assistant")
            if "ALL DONE" in said and not groups_live:
                finished = True
                break
            if await watch.quiet():
                print(f"  · nothing in flight for {IDLE_END:.0f}s — the master stopped", flush=True)
                break
        final = await live_ids(root)
        ledger.observe(time.monotonic() - t0, final)
        sampler.cancel()
        if not kick.done():
            kick.cancel()
        await conn.close()

        # ── score ──
        print("\n" + "=" * (W + 4))
        c1 = 1 <= ledger.max_live() <= MAX_GROUPS
        print(f"    1. bound held                     {'OK' if c1 else 'NO'} max_live={ledger.max_live()} max_groups={MAX_GROUPS}")
        orphans = ledger.orphans(final)
        c2 = not orphans
        print(f"    2. no orphans                     {'OK' if c2 else 'NO'} orphans={orphans}")
        results = {}
        for t in tasks:
            ok, line = run_suite(t["dir"])
            untouched = all((Path(t["dir"]) / f).exists() and (Path(t["dir"]) / f).read_text() == body
                            for f, body in protected[t["name"]].items())
            results[t["name"]] = (ok and untouched, line)
        c3 = all(v[0] for v in results.values())
        print(f"    3. every suite passes, untouched  {'OK' if c3 else 'NO'} "
              f"{sum(v[0] for v in results.values())}/{len(results)}")
        for name, (ok, line) in results.items():
            print(f"         {name:<24} {'pass' if ok else 'FAIL'} {line}")
        biggest = max_turn_input(prompt, await rows(root, MASTER))
        c4 = 0 < biggest <= MASTER_INPUT_CHARS_MAX
        print(f"    4. master prompt bounded          {'OK' if c4 else 'NO'} "
              f"max_turn_input={biggest} chars limit={MASTER_INPUT_CHARS_MAX} prompt={len(prompt)}")

        print("\n  ── observed ──")
        print(f"  wall (ledger)          : {wall_seconds(ledger):.0f}s  finished={finished}")
        for g, (a, b) in ledger.lifetimes().items():
            print(f"  group {g:<3} lived       : {a:6.0f}s → {b:6.0f}s ({b - a:.0f}s)")
        agents = [MASTER, *(w for w in worker_ids() if any(w in ids for _, ids in ledger.samples))]
        for aid in agents:
            conv = await rows(root, aid)
            turns = sum(1 for r in conv if r.get("role") == "assistant")
            print(f"  turns {aid:<16}: {turns}")
        metered = meter_rows()
        if metered:
            from helpers.meter_report import _price_env, request_usd

            prices = _price_env()
            pt = sum(r.get("prompt_tokens") or 0 for r in metered)
            cr = sum(r.get("cached_tokens") or 0 for r in metered)
            ct = sum(r.get("completion_tokens") or 0 for r in metered)
            usd = [request_usd(r, prices)[0] for r in metered]
            cost = f"${sum(u for u in usd if u is not None):.4f}" if any(u is not None for u in usd) else "unpriced"
            # THE METER TAGS PER TEST, not per agent: these are every agent's requests.
            print(f"  meter (all agents)     : req={len(metered)} prompt_tok={pt} cached_tok={cr} "
                  f"cache_hit={cr / pt if pt else 0:.1%} compl_tok={ct} cost={cost}")
        else:
            print("  meter                  : not metered (E2E_METER=1 adds tokens, cache hits and cost)")
        ok = c1 and c2 and c3 and c4
        print(f"\n  RESULT: {'PASS — the master ran its groups within the bound and cleaned up' if ok else 'FAIL'}")
        verdict(ok)
        sys.exit(0 if ok else 1)
    finally:
        await close()


if __name__ == "__main__":
    asyncio.run(main())
