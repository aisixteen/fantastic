"""HARNESS SELFTEST for `build` — no model, no inference, no cost.

    cd src/lib/python && uv run python -u ../../../e2e/tests/agentic/E20_build/selftest.py

Never ship a probe you have not proven is passable. This plants a HAND-WRITTEN
reference program into the same fixture the probe builds, and runs the same three
checks against it. Two things must come out of it:

  * the task is SOLVABLE — some program satisfies all three checks, so a model
    failing the probe is a fact about the model, not the harness;
  * the CANARY actually fires — check 3 is the whole point, and a canary that
    cannot go green is worse than no canary at all.

The NEW execution model (no `code` field, no shims): a durable program is a FILE
plus a command. The reference writes a real `main.py` — the wire connector plus a
body that totals the readings off disk and stores the total — then runs it as
`<python> main.py` on a command runner. The canary re-runs the SAME command after
a fresh file lands.

TWO SCENARIOS, because there are two shapes a passing run has:

  * `own`  — the model builds ITS OWN `exec_bridge.tools` worker and runs the
    program there, the route the kernel's root readme advertises ("A WORKER THAT
    COMPUTES is an `exec_bridge.tools` agent … that owns a program file"). Measured
    2026-09-21: 2 live runs of 3 did exactly this, stored the correct total, and
    scored `jobs=0` — the scorer looked only at the operator's pre-wired leg.
  * `leg`  — the model reflects the operator's pre-wired `runner` and runs there.

Both must score the same, and each must be scored on the RIGHT agent. Scoring goes
through the probe's own `pick_program_runner` / `_last_successful_job` / `job_cwd`,
never a local copy — a second copy of the scoring is the drift the emergence README
records twice already.

ORDER MATTERS: `own` runs FIRST. `exec_bridge` keeps its job table in a module
global keyed by OWNER ID, and both scenarios wire an agent called `runner` in the
same process; running `leg` first would leave `runner` holding jobs that the `own`
scenario's picker would then legitimately prefer. Fresh kernel, fresh workdir,
still one `_jobs` — so the idle-leg scenario goes first.
"""

from __future__ import annotations

import asyncio
import os
import re
import secrets
import sys
import tempfile
from pathlib import Path

# e2e bootstrap — walk up to `.e2e-root`; no hard-coded depth, so moving this
# file into a folder can never silently break its imports again.
sys.path.insert(0, str(next(d for d in Path(__file__).resolve().parents
                            if (d / ".e2e-root").exists())))
from helpers import verdict

sys.argv = [sys.argv[0]]

from tests.agentic.E20_build import probe as P  # noqa: E402
from tests.agentic.E20_build.probe import CANARY_FILE, EXECUTOR_LEG  # noqa: E402

# The id the model's own worker gets here. Named after the one a live run actually
# chose (2026-09-21) rather than something like `my_exec`, so the scenario reads as
# the run it is reproducing.
OWN_LEG = "totaller"

# The reference solution: a PLAIN program that reads the readings OFF DISK (read_stream
# returns raw bytes, which do not cross the JSON control wire — disk is the byte path a
# program has), totals them, and stores the total on the memory agent over its wire.
# No callback, no loop, no daemon: the task asked for a program, not a service, and the
# canary proves the PROGRAM works by re-running it.
REFERENCE = """
import os
_t = 0
for _n in os.listdir("readings"):
    if _n.endswith(".txt"):
        try:
            with open(os.path.join("readings", _n)) as _f:
                _t += int(_f.read().strip())
        except (ValueError, OSError):
            pass
with open("computed_total.txt", "w") as _out:
    _out.write(str(_t))
"""


async def find_value(root, mem_id, want):
    """Memory is FILES: word-bounded scan of the workdir (E20 scorer's twin)."""
    _ = mem_id
    import re as _re
    pat = _re.compile(rf"\b{want}\b")
    for dirpath, dirnames, filenames in os.walk("."):
        dirnames[:] = [d for d in dirnames if d not in ("readings", ".fantastic", ".ckpt", "__pycache__")]
        for name in sorted(filenames):
            if name.endswith((".py", ".jsonl")) or ".tmp." in name:
                continue
            fp = os.path.join(dirpath, name)
            try:
                text = open(fp, errors="ignore").read()
            except OSError:
                continue
            if pat.search(text):
                return fp, text.strip()[:120], True
    return None, None, False


async def wait_for(root, mem_id, want, secs):
    waited = 0.0
    while waited < secs:
        _k, _v, ok = await find_value(root, mem_id, want)
        if ok:
            return True
        await asyncio.sleep(1.0)
        waited += 1.0
    return False


async def wait_for_scored_job(root, secs: float):
    """Poll the PROBE'S OWN picker until it sees a completed job — or give up.

    The program writes its file and then exits, so the value can be on disk a beat
    before the bridge has reaped the process and recorded `exit_code`. Check 1 reads
    `exit_code == 0`, so the selftest has to wait for the same fact the scorer reads
    rather than for the side effect that precedes it.
    """
    waited = 0.0
    while True:
        rid, jobs = await P.pick_program_runner(root)
        job = P._last_successful_job(jobs)
        if job is not None or waited >= secs:
            return rid, jobs, job
        await asyncio.sleep(1.0)
        waited += 1.0


async def scenario(name: str, own: bool) -> bool:
    """One full fixture + reference solution + the probe's three checks.

    `own` picks WHICH exec agent runs the program: the model's own worker, or the
    operator's pre-wired leg. Everything else — the fixture, the program, the
    scoring path — is identical, which is the whole point of running both.
    """
    work = tempfile.mkdtemp(prefix=f"buildselftest_{name}_")
    os.chdir(work)
    readings = {f"reading_{i}.txt": secrets.randbelow(9000) + 1000 for i in range(1, 4)}
    canary_value = secrets.randbelow(9000) + 1000

    from _testkit import boot_root
    from exec_bridge._connector import CONNECTOR_SRC

    root = boot_root()
    store = (await root.send([{"target": root.id, "payload": {
            "type": "create_agent",
            "parent_id": root.id,
            "handler_module": "file_bridge.tools",
            "id": "kernel_store",
            "root": ".",  # the run dir — main.py lands beside readings/ and in runner cwd
            "ingress_rule": "allow_all_ingress",
        }}]))[0]["result"]
    os.makedirs("readings", exist_ok=True)
    for fname, val in readings.items():
        with open(os.path.join("readings", fname), "w") as fh:
            fh.write(f"{val}\n")
    (await root.send([{"target": root.id, "payload": {
            "type": "create_agent",
            "parent_id": root.id,
            "handler_module": "file_bridge.tools",
            "id": "readings",
            "root": "readings",
            "ingress_rule": "allow_all_ingress",
        }}]))[0]["result"]
    mem = (await root.send([{"target": root.id, "payload": {
            "type": "create_agent",
            "parent_id": root.id,
            "handler_module": "text_file.tools",
            "id": "totals",
            "target_id": store["id"],
            "can_call": {store["id"]: "*"},
            # accept the running program's `set`, from EITHER leg it might arrive on
            "callable_by": {EXECUTOR_LEG: "*", OWN_LEG: "*"},
        }}]))[0]["result"]
    # The operator's half: the command runner. Wired in BOTH scenarios — in `own` it
    # sits there idle exactly as it does in the live runs this reproduces.
    await P.wire_executor_leg(root)

    if own:
        # THE MODEL'S HALF, as a live model actually builds it: its own exec worker,
        # its own ingress, its own command gate. The probe's picker must find THIS one
        # even though the operator's leg exists and is perfectly usable.
        exec_id = (await root.send([{"target": root.id, "payload": {
                "type": "create_agent",
                "parent_id": root.id,
                "handler_module": "exec_bridge.tools",
                "id": OWN_LEG,
                "cwd": os.getcwd(),
                "ingress_rule": "allow_all_ingress",
                "callable_by": {"ai": "*", "rest": "*", "web": "*"},
                "allowed": [rf"{re.escape(sys.executable)} \S+"],
                "can_call": {"totals": "*"},
            }}]))[0]["result"]["id"]
    else:
        exec_id = EXECUTOR_LEG

    total = sum(readings.values())
    new_total = total + canary_value
    print(f"\n  --- scenario {name!r}: the program runs on {exec_id!r} ---")
    print(f"  true total = {total}   after canary = {new_total}")

    # DISCOVERED, not hardcoded — the same path the probe's scoring and a model use.
    _leg, pats = await P.discover_command_runner(root)
    print(f"  discovered runner: {_leg}   command gate: {pats}")

    # THE PROGRAM AS A FILE: the wire connector + the body, written to disk, then run as
    # `<python> main.py`. Including the connector is how a spawned program gets a `kernel`
    # now that nothing is auto-prepended (the shim mechanism is gone).
    with open("main.py", "w") as fh:
        fh.write(CONNECTOR_SRC + REFERENCE)
    started = (await root.send([{"target": exec_id, "payload": {
            "type": "start",
            "command": sys.executable,
            "args": ["main.py"],
        }}]))[0]["result"]
    print(f"     start={started}")

    # 1. artifact — THROUGH THE PROBE'S SCORER, not a local `started.job_id` read.
    #    That older check passed on a job the probe itself would have scored `jobs=0`,
    #    which is precisely the defect this selftest now has to be able to catch.
    ran_on, jobs, job = await wait_for_scored_job(root, 60)
    c1 = job is not None and ran_on == exec_id
    print(f"  1. artifact exists   {'OK' if c1 else 'NO'}  "
          f"runner={ran_on!r} (want {exec_id!r}) jobs={len(jobs)} "
          f"ran={None if job is None else [job.get('command'), job.get('args')]}")

    # 2. correct total (the program computes once at startup)
    c2 = await wait_for(root, mem["id"], total, 60)
    print(f"  2. total correct     {'OK' if c2 else 'NO'}  expected={total}")

    # 3. THE CANARY — a file the program has never seen; re-issue the recorded job
    #    VERBATIM on the agent the scorer picked, cwd included, exactly as the probe does.
    with open(os.path.join("readings", CANARY_FILE), "w") as fh:
        fh.write(f"{canary_value}\n")
    c3 = False
    if job is not None:
        cwd = await P.job_cwd(root, ran_on, job["job_id"])
        (await root.send([{"target": ran_on, "payload": {
                "type": "start",
                "command": job["command"],
                "args": job.get("args") or [],
                **({"cwd": cwd} if cwd else {}),
            }}]))[0]["result"]
        c3 = await wait_for(root, mem["id"], new_total, 90)
    print(f"  3. it RUNS (canary)  {'OK' if c3 else 'NO'}  expected={new_total}")

    ok = c1 and c2 and c3
    print(f"  scenario {name!r}: {'PASS' if ok else 'FAIL'}")
    return ok


async def main():
    # `own` FIRST — see the module docstring: one process, one `_jobs` table, two
    # kernels that both call their operator leg `runner`.
    own_ok = await scenario("own", own=True)
    leg_ok = await scenario("leg", own=False)

    ok = own_ok and leg_ok
    print(f"\n  own-agent run: {'PASS' if own_ok else 'FAIL'}   "
          f"operator-leg run: {'PASS' if leg_ok else 'FAIL'}")
    print(f"  HARNESS: {'USABLE — the probe is passable BOTH ways' if ok else 'BROKEN'}")
    verdict(ok)
    sys.exit(0 if ok else 1)


asyncio.run(main())
