"""HARNESS SELFTEST for `live_subai.py` — no model, no inference, no cost.

    cd src/lib/python && uv run python -u ../../../tmp/live_subai_selftest.py

Builds the sub-AI BY HAND — the structure the probe asks a model to discover — and runs
the same four checks. Two things must come out of it:

  * the task is SOLVABLE, so a model failing the probe is a fact about the model;
  * checks 3 and 4 actually go green, since "it runs unattended" and "it follows a new
    reading" are the whole point and a canary that cannot fire is worse than none.

The scar this exists for: `live_llamacpp.py` asserted `<think>` on a surface that strips
it and the FIRST run of a healthy system scored FAIL; and `live_build.py`'s first canary
demanded a live service the task never asked for. The harness gets tested first now.
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
from helpers import (  # noqa: E402
    BASELINE, Limits, ProxyReader, Watch, distinct_calls, verdict,
)
from backends import backend as _resolve_backend  # noqa: E402
sys.argv = [sys.argv[0]]

from tests.agentic_advanced.E30_subai import probe as P  # noqa: E402  — reuse its fixture + scoring verbatim

# The reference sub-AI, written by hand. A PLAIN program: read, total, store, exit.
# It stores a RICH result on purpose — a bare int would leave the harness's
# nested-value path unexercised, and that path is the one that mis-scored opus.
# It needs no keep-alive and no `on_message` — the clock re-runs it, which is what
# makes the structure standing rather than a live daemon.
TOTALLER = """
listing = kernel.send("readings", {"type": "list", "path": ""})
total = 0
for f in listing.get("files", []):
    r = kernel.send("readings", {"type": "read", "path": f["path"]})
    total += int(str(r.get("content", "0")).strip())
# A RICH result, matching what opus-5 actually stored on 2026-08-05 — the harness
# must accept a total nested in a result dict, not only a bare number.
kernel.send("totals", {"type": "set", "key": "reading_total", "value": {
    "total": total, "count": len(listing.get("files", [])), "source": "readings",
    "skipped": [],
}})
"""


async def main():
    os.chdir(tempfile.mkdtemp(prefix="subai_self_"))
    from _testkit import boot_root

    root = boot_root()
    store = await P.build_fixture(root)

    total = sum(P.READINGS.values())
    new_total = total + P.CANARY_VALUE
    print(f"  true total = {total}   after canary = {new_total}")

    # ── the structure a model is asked to discover ──
    # A sub-agent to own the work…
    (await root.send([{"target": root.id, "payload": {
            "type": "create_agent",
            "parent_id": root.id,
            "handler_module": "yaml_state.tools",
            "id": "monitor",
            "file_bridge_id": store["id"],
            "description": "The sub-AI: owns the totalling job.",
        }}]))[0]["result"]
    # …with a durable script CHILD whose persisted code IS the program.
    #
    # The executor is DISCOVERED the way a model must discover it, not hardcoded.
    # Hardcoding it is how this selftest passed while the probe was unwinnable: it
    # proved a solution EXISTS while stepping over whether the solution is FINDABLE,
    # which is the more important half of an emergence probe. opus failed the probe on
    # 2026-08-05 purely because nothing in the default reflect named an executor.
    # ONE definition of "what counts as an executor", shared with the probe's own
    # scoring — so the selftest cannot discover one module while `score` looks for
    # another. That split is exactly what made this selftest pass on 2026-08-05 while
    # the probe it validates was unwinnable.
    executor = sorted(await P.advertised_executors(root))[0]
    # The interpreter is the SECOND unguessable fact: the advertised executor spawns
    # only what a record allowlists and has no default. Discovered the same way — from
    # the fixture's executor leg, not from `sys.executable`.
    interpreter = (await P.advertised_interpreters(root))[0]
    print(f"  discovered executor: {executor}   interpreter: {interpreter}")
    (await root.send([{"target": root.id, "payload": {
            "type": "create_agent",
            "parent_id": root.id,
            "handler_module": executor,
            "id": "totaller",
            "parent_id": "monitor",
            "code": TOTALLER,
            # THE EDGES THE PROGRAM ACTUALLY USES. The gate is two-sided and
            # deny-all: the structural baseline covers `$self`/`$parent`/
            # `$children`, and `readings`/`totals` are neither — they hang off
            # the ROOT, while this agent hangs off `monitor`. Without these the
            # program's first call comes back
            #   {"reason":"unauthorized","error":"totaller may not call 'list' on readings"}
            # and it dies indexing a refusal, storing nothing. That is EXACTLY
            # how this harness failed from the day the two-sided gate landed
            # until 2026-08-28: `stored=None`, no visible cause, "HARNESS: BROKEN".
            #
            # Named verbs, never "*": a wildcard here would hide a missing-grant
            # failure in the PROBE, and whether a model wires authority is half
            # of what the probe measures.
            "can_call": {
                "readings": ["list", "read"],
                "totals": ["set", "read"],
            },
            # Load-bearing, all three (measured 2026-08-07): no `interpreters` and
            # `start` refuses; no `ingress_rule` and the leg is SEALED, so the boot that
            # would run it is denied and that reply is discarded — the tool is silently
            # inert; no `shim` and the program dies on
            # "NameError: name 'kernel' is not defined". The allowlist is PER RECORD —
            # nothing is inherited from the parent.
            "interpreters": [interpreter],
            "shim": "python",
            "ingress_rule": "allow_all_ingress",
        }}]))[0]["result"]
    # THE INGRESS HALVES. An edge exists only when BOTH ends grant it, so the
    # callees must accept this caller — the fixture opened them to `ai`/`brain`,
    # never to what the builder CREATES. Operator sends: the harness is standing
    # in for the model, and a model would have to do exactly this.
    for callee, verbs in (("readings", ["list", "read"]), ("totals", ["set", "read"])):
        rec = (await root.send([{"target": root.id, "payload": {
                "type": "reflect", "tree": "none"}}]))[0]["result"]
        _ = rec
        cur = (await root.send([{"target": root.id, "payload": {
                "type": "get", "id": callee}}]))[0]["result"]
        accepts = dict(cur.get("callable_by") or {})
        accepts["totaller"] = verbs
        r = (await root.send([{"target": root.id, "payload": {
                "type": "update_agent", "id": callee, "callable_by": accepts,
            }}]))[0]["result"]
        assert not r.get("error"), f"grant ingress on {callee}: {r}"

    # The clock drives the script, so that edge needs granting too — in BOTH
    # directions, like any other.
    cur = (await root.send([{"target": root.id, "payload": {
            "type": "get", "id": "totaller"}}]))[0]["result"]
    accepts = dict(cur.get("callable_by") or {})
    accepts["clock"] = ["start"]
    (await root.send([{"target": root.id, "payload": {
            "type": "update_agent", "id": "totaller", "callable_by": accepts}}]))[0]["result"]
    cur = (await root.send([{"target": root.id, "payload": {
            "type": "get", "id": "clock"}}]))[0]["result"]
    reach = dict(cur.get("can_call") or {})
    reach["totaller"] = ["start"]
    (await root.send([{"target": root.id, "payload": {
            "type": "update_agent", "id": "clock", "can_call": reach}}]))[0]["result"]

    # …driven by the clock, aimed at its own work.
    r = (await root.send([{"target": "clock", "payload": {
            "type": "schedule",
            "target": "totaller",
            "payload": {"type": "start"},
            "interval_seconds": P.TICK,
        }}]))[0]["result"]
    assert not r.get("error"), f"schedule failed: {r}"
    print(f"  scheduled: {r.get('schedule_id')} every {P.TICK}s -> totaller.start")

    ok = await P.score(root, store["id"], total, new_total, ["hand-built reference"])
    print(f"\n  HARNESS: {'USABLE — the probe is passable' if ok else 'BROKEN'}")
    verdict(ok)
    sys.exit(0 if ok else 1)


asyncio.run(main())
