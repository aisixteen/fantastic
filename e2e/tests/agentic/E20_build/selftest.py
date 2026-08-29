"""HARNESS SELFTEST for `live_build.py` — no model, no inference, no cost.

    cd src/lib/python && uv run python -u ../../../tmp/live_build_selftest.py

Never ship a probe you have not proven is passable. This plants a HAND-WRITTEN
reference program into the same fixture `live_build.py` builds and runs the same
three checks against it. Two things have to come out of it:

  * the task is SOLVABLE — some program exists that satisfies all three checks, so
    a model failing the probe is a fact about the model and not about the harness;
  * the CANARY actually fires — check 3 is the whole point of the probe, and a
    canary that cannot go green is worse than no canary at all.

The lesson this exists to encode: earlier the same day, `live_llamacpp.py` asserted
`<think>` on a surface that strips it, and the FIRST run of a healthy system scored
FAIL. The harness gets tested first now.
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
from helpers import (  # noqa: E402
    BASELINE, Limits, ProxyReader, Watch, distinct_calls, verdict,
)
from backends import backend as _resolve_backend  # noqa: E402
sys.argv = [sys.argv[0]]

from tests.agentic.E20_build import probe as P  # noqa: E402  — share ONE definition of "an executor"

READINGS = {f"reading_{i}.txt": secrets.randbelow(9000) + 1000 for i in range(1, 4)}
CANARY_VALUE = secrets.randbelow(9000) + 1000

# The reference solution: a PLAIN program. It reads, totals, stores, and exits. No
# callback, no loop, no daemon — the task asked for a program, not a service. The
# canary re-RUNS it (`{"type":"start"}` falls back to the record's persisted `code`,
# tools.py:354), which is what proves the CODE works rather than proving a process
# happens to still be sitting there.
REFERENCE = """
def _total():
    listing = kernel.send("readings", {"type": "list", "path": ""})
    total = 0
    for f in listing.get("files", []):
        r = kernel.send("readings", {"type": "read", "path": f["path"]})
        total += int(str(r.get("content", "0")).strip())
    kernel.send("totals", {"type": "set", "key": "total", "value": total})
    return total

_total()
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

    root = boot_root()
    store = (await root.send([{"target": root.id, "payload": {
            "type": "create_agent",
            "parent_id": root.id,
            "handler_module": "file_bridge.tools",
            "id": "kernel_store",
            "root": ".fantastic",
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
            "callable_by": {"totaller": "*"},
        }}]))[0]["result"]
    mem = (await root.send([{"target": root.id, "payload": {
            "type": "create_agent",
            "parent_id": root.id,
            "handler_module": "yaml_state.tools",
            "id": "totals",
            "file_bridge_id": store["id"],
            "callable_by": {"totaller": "*"},
        }}]))[0]["result"]
    # The operator's half: one OPEN executor leg with an interpreter allowlist. Shared
    # with the probe so the fixture cannot drift from the one the model is graded in.
    await P.wire_executor_leg(root)

    total = sum(READINGS.values())
    new_total = total + CANARY_VALUE
    print(f"  true total = {total}   after canary = {new_total}")

    # DISCOVERED, not hardcoded — the same path the probe's own scoring uses, and the
    # same one a model has. A hardcoded module here would prove a solution EXISTS while
    # stepping over whether it is FINDABLE, which is the more important half.
    executor = sorted(await P.advertised_executors(root))[0]
    interpreter = (await P.advertised_interpreters(root))[0]
    print(f"  discovered executor: {executor}   interpreter: {interpreter}")
    built = (await root.send([{"target": root.id, "payload": {
            "type": "create_agent",
            "parent_id": root.id,
            "handler_module": executor,
            "id": "totaller",
            "code": REFERENCE,
            # Two-sided gate (2026-08-24): the reference predates it and granted
            # nothing, so its sends were deny-all'd and the selftest reported the
            # HARNESS broken. The reference builder needs the same grants a model
            # must discover: egress here, ingress on the two targets below.
            "can_call": {"readings": "*", "totals": "*"},
            # The executor is an io leg with an interpreter ALLOWLIST, and all three of
            # these are load-bearing — measured 2026-08-07, each one omitted:
            #   no `interpreters` → start refuses, "no interpreters are allowlisted"
            #   no `ingress_rule` → SEALED: create looks fine, boot is denied and that
            #                       reply is discarded, so the tool silently never runs
            #   no `shim`         → the program runs and dies on
            #                       "NameError: name 'kernel' is not defined"
            "interpreters": [interpreter],
            "shim": "python",
            "ingress_rule": "allow_all_ingress",
        }}]))[0]["result"]

    # 1. artifact
    listing = (await root.send([{"target": "kernel", "payload": {"type": "list_agents"}}]))[0]["result"]
    records = {a["id"]: a for a in (listing.get("agents") or [])}
    code = records.get(built["id"], {}).get("code") or ""
    c1 = bool(code.strip())
    print(f"  1. artifact exists   {'OK' if c1 else 'NO'}  code={len(code)} chars")

    # 2. correct total (the script computes once at startup)
    c2 = await wait_for(root, mem["id"], total, 60)
    print(f"  2. total correct     {'OK' if c2 else 'NO'}  expected={total}")

    # 3. THE CANARY — a file the program has never seen
    with open(os.path.join("readings", "reading_4.txt"), "w") as fh:
        fh.write(f"{CANARY_VALUE}\n")
    (await root.send([{"target": built["id"], "payload": {"type": "start"}}]))[0]["result"]
    c3 = await wait_for(root, mem["id"], new_total, 90)
    print(f"  3. it RUNS (canary)  {'OK' if c3 else 'NO'}  expected={new_total}")

    ok = c1 and c2 and c3
    print(f"\n  HARNESS: {'USABLE — the probe is passable' if ok else 'BROKEN'}")
    verdict(ok)
    sys.exit(0 if ok else 1)


asyncio.run(main())
