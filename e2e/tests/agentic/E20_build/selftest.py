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
`<python> main.py` on the operator's command runner. The canary re-runs the SAME
command after a fresh file lands.
"""

from __future__ import annotations

import asyncio
import os
import secrets
import sys
import tempfile
from pathlib import Path

# e2e bootstrap — walk up to `.e2e-root`; no hard-coded depth, so moving this
# file into a folder can never silently break its imports again.
sys.path.insert(0, str(next(d for d in Path(__file__).resolve().parents
                            if (d / ".e2e-root").exists())))
from helpers import verdict  # noqa: E402
from backends import backend as _resolve_backend  # noqa: E402
sys.argv = [sys.argv[0]]

from tests.agentic.E20_build import probe as P  # noqa: E402  — share ONE fixture + discovery
from tests.agentic.E20_build.probe import CANARY_FILE, EXECUTOR_LEG  # noqa: E402

READINGS = {f"reading_{i}.txt": secrets.randbelow(9000) + 1000 for i in range(1, 4)}
CANARY_VALUE = secrets.randbelow(9000) + 1000

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
kernel.send("totals", {"type": "set", "key": "total", "value": _t})
"""


async def find_value(root, mem_id, want):
    entries = ((await root.send([{"target": mem_id, "payload": {"type": "keys"}}]))[0]["result"]).get("keys") or []
    keys = [e["key"] if isinstance(e, dict) else e for e in entries]
    for k in keys:
        v = ((await root.send([{"target": mem_id, "payload": {"type": "read", "key": k}}]))[0]["result"]).get("value")
        if v is not None and str(v).strip() == str(want):
            return k, str(v).strip(), True
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


async def main():
    os.chdir(tempfile.mkdtemp(prefix="buildselftest_"))
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
    for name, val in READINGS.items():
        with open(os.path.join("readings", name), "w") as fh:
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
            "handler_module": "durable_memory.tools",
            "id": "totals",
            "file_bridge_id": store["id"],
            # accept the running program's `set`, which arrives from the runner leg
            "callable_by": {EXECUTOR_LEG: "*"},
        }}]))[0]["result"]
    # The operator's half: the command runner (its can_call->totals is set by the helper).
    runner_id = await P.wire_executor_leg(root)

    total = sum(READINGS.values())
    new_total = total + CANARY_VALUE
    print(f"  true total = {total}   after canary = {new_total}")

    # DISCOVERED, not hardcoded — the same path the probe's scoring and a model use.
    _leg, pats = await P.discover_command_runner(root)
    print(f"  discovered runner: {_leg}   command gate: {pats}")

    # THE PROGRAM AS A FILE: the wire connector + the body, written to disk, then run as
    # `<python> main.py`. Including the connector is how a spawned program gets a `kernel`
    # now that nothing is auto-prepended (the shim mechanism is gone).
    with open("main.py", "w") as fh:
        fh.write(CONNECTOR_SRC + REFERENCE)
    started = (await root.send([{"target": runner_id, "payload": {
            "type": "start",
            "command": sys.executable,
            "args": ["main.py"],
        }}]))[0]["result"]

    # 1. artifact — the command ran
    c1 = bool(started.get("job_id")) and started.get("status") == "running"
    print(f"  1. artifact runs     {'OK' if c1 else 'NO'}  start={started}")

    # 2. correct total (the program computes once at startup)
    c2 = await wait_for(root, mem["id"], total, 60)
    print(f"  2. total correct     {'OK' if c2 else 'NO'}  expected={total}")

    # 3. THE CANARY — a file the program has never seen; re-run the SAME command
    with open(os.path.join("readings", CANARY_FILE), "w") as fh:
        fh.write(f"{CANARY_VALUE}\n")
    (await root.send([{"target": runner_id, "payload": {
            "type": "start",
            "command": sys.executable,
            "args": ["main.py"],
        }}]))[0]["result"]
    c3 = await wait_for(root, mem["id"], new_total, 90)
    print(f"  3. it RUNS (canary)  {'OK' if c3 else 'NO'}  expected={new_total}")

    ok = c1 and c2 and c3
    print(f"\n  HARNESS: {'USABLE — the probe is passable' if ok else 'BROKEN'}")
    verdict(ok)
    sys.exit(0 if ok else 1)


asyncio.run(main())
