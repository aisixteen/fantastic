"""HARNESS SELFTEST for `subai` — no model, no inference, no cost.

    cd src/lib/python && uv run python -u ../../../e2e/tests/agentic_advanced/E30_subai/selftest.py

Builds the sub-AI BY HAND — the structure the probe asks a model to discover — and runs
the same four checks. Two things must come out of it:

  * the task is SOLVABLE, so a model failing the probe is a fact about the model;
  * checks 3 and 4 actually go green, since "it runs unattended" and "it follows a new
    reading" are the whole point and a canary that cannot fire is worse than none.

The NEW execution model (no `code` field, no shims): the specialist owns a COMMAND-RUNNER
child; the durable program is a FILE (the wire connector + a body) that reads the readings
off disk and stores the total over the control wire; the clock re-runs that command.
"""

from __future__ import annotations

import asyncio
import os
import sys
import tempfile
from pathlib import Path

# e2e bootstrap — walk up to `.e2e-root`; no hard-coded depth, so moving this
# file into a folder can never silently break its imports again.
sys.path.insert(0, str(next(d for d in Path(__file__).resolve().parents
                            if (d / ".e2e-root").exists())))
from helpers import verdict

sys.argv = [sys.argv[0]]

from tests.agentic_advanced.E30_subai import probe as P  # noqa: E402

PROG_FILE = "totaller_prog.py"

# The reference program body (the wire connector is prepended when written to disk).
# A PLAIN program: read the readings OFF DISK (read_stream returns raw bytes, which do
# not cross the JSON control wire — disk is the byte path a program has), total, store
# a RICH result over the wire, exit. No keep-alive, no on_message — the clock re-runs it,
# which is what makes the structure standing rather than a live daemon.
BODY = """
import os
_files = [f for f in os.listdir("readings") if f.endswith(".txt")]
_total = 0
for _f in _files:
    try:
        with open(os.path.join("readings", _f)) as _fh:
            _total += int(_fh.read().strip())
    except (ValueError, OSError):
        pass
# A RICH result: the harness must accept a total nested in a result dict, not only a
# bare number (matching what opus-5 actually stored on 2026-08-05).
kernel.send("totals", {"type": "set", "key": "reading_total", "value": {
    "total": _total, "count": len(_files), "source": "readings", "skipped": [],
}})
"""


async def main():
    os.chdir(tempfile.mkdtemp(prefix="subai_self_"))
    from _testkit import boot_root
    from exec_bridge._connector import CONNECTOR_SRC

    root = boot_root()
    store = await P.build_fixture(root)

    total = sum(P.READINGS.values())
    new_total = total + P.CANARY_VALUE
    print(f"  true total = {total}   after canary = {new_total}")

    # DISCOVERED the way a model must: the fixture's `runner` leg advertises the command
    # gate; the specialist's own compute child runs the SAME interpreter against a file.
    rr = (await root.send([{"target": P.EXECUTOR_LEG, "payload": {"type": "reflect"}}]))[0]["result"]
    pattern = (rr.get("allowed") or [None])[0]
    print(f"  command gate on {P.EXECUTOR_LEG}: {pattern}")

    # ── the structure a model is asked to discover ──
    # A sub-agent to own the work…
    (await root.send([{"target": root.id, "payload": {
            "type": "create_agent",
            "parent_id": root.id,
            "handler_module": "text_file.tools",
            "id": "monitor",
            "target_id": store["id"],
            "can_call": {store["id"]: "*"},
            "description": "The sub-AI: owns the totalling job.",
        }}]))[0]["result"]
    # …with a COMMAND-RUNNER child under it (a compute worker the specialist owns).
    (await root.send([{"target": root.id, "payload": {
            "type": "create_agent",
            "handler_module": P.EXEC_MODULE,
            "id": "totaller",
            "parent_id": "monitor",
            "cwd": os.getcwd(),  # so `<interp> <file>` and open('readings/..') resolve
            # The program stores over the wire → the send lands from this leg; it needs
            # egress to totals (readings it reads from DISK, no grant needed).
            "can_call": {"totals": ["set", "read"]},
            "allowed": [pattern],
            "ingress_rule": "allow_all_ingress",
        }}]))[0]["result"]
    # THE INGRESS HALF: an edge exists only when BOTH ends grant it — totals must accept
    # this caller. (The fixture opened it to ai/brain, never to what the builder makes.)
    cur = (await root.send([{"target": root.id, "payload": {"type": "get", "id": "totals"}}]))[0]["result"]
    accepts = dict(cur.get("callable_by") or {})
    accepts["totaller"] = ["set", "read"]
    r = (await root.send([{"target": root.id, "payload": {
            "type": "update_agent", "id": "totals", "callable_by": accepts}}]))[0]["result"]
    assert not r.get("error"), f"grant ingress on totals: {r}"

    # THE PROGRAM AS A FILE: connector + body on disk, run as `<interp> <file>`.
    with open(PROG_FILE, "w") as fh:
        fh.write(CONNECTOR_SRC + BODY)
    started = (await root.send([{"target": "totaller", "payload": {
            "type": "start",
            "command": sys.executable,
            "args": [PROG_FILE],
        }}]))[0]["result"]
    assert started.get("job_id"), started

    # The clock drives the child, so that edge needs granting too — both directions.
    cur = (await root.send([{"target": root.id, "payload": {"type": "get", "id": "totaller"}}]))[0]["result"]
    accepts = dict(cur.get("callable_by") or {})
    accepts["clock"] = ["start"]
    (await root.send([{"target": root.id, "payload": {
            "type": "update_agent", "id": "totaller", "callable_by": accepts}}]))[0]["result"]
    cur = (await root.send([{"target": root.id, "payload": {"type": "get", "id": "clock"}}]))[0]["result"]
    reach = dict(cur.get("can_call") or {})
    reach["totaller"] = ["start"]
    (await root.send([{"target": root.id, "payload": {
            "type": "update_agent", "id": "clock", "can_call": reach}}]))[0]["result"]

    # …driven by the clock, aimed at its own work.
    r = (await root.send([{"target": "clock", "payload": {
            "type": "schedule",
            "target": "totaller",
            "payload": {"type": "start", "command": sys.executable, "args": [PROG_FILE]},
            "interval_seconds": P.TICK,
        }}]))[0]["result"]
    assert not r.get("error"), f"schedule failed: {r}"
    print(f"  scheduled: {r.get('schedule_id')} every {P.TICK}s -> totaller.start")

    ok = await P.score(root, store["id"], total, new_total, ["hand-built reference"])
    print(f"\n  HARNESS: {'USABLE — the probe is passable' if ok else 'BROKEN'}")
    verdict(ok)
    sys.exit(0 if ok else 1)


asyncio.run(main())
