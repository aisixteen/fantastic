"""LIVE TEST 08 — SUBAI + GRAPH SHAPE: the combined composition — the sub-agent
task (probe 06) with the state-graph decomposition ask (probe 07's variable).
No graph library exists anywhere in this codebase; the SHAPE is the variable.

    cd src/lib/python && uv run python -u ../../../e2e/run.py --group agentic_advanced --id subai_graph --lanes 1

## What this adds over probes 05 and 06

Probe 06 asked for one program on `exec_bridge`. Probe 05 asked for a specialist with
CHILD workers wired to a scheduler. Neither asked for all of it at once, and neither put
an AI agent inside the thing being built. So THIS combination has never run:

  * the totalling logic as a state graph — nodes, edge map, self-written runner,
  * inside a SPECIALIST that owns child workers (probe 05's shape),
  * wired to the `clock` scheduler so it re-checks unattended (probe 05's clock),
  * consulting the `brain` AI agent by id (new here — inference as a WIRED UNIT),
  * on `exec_bridge`, the command runner (a program is a FILE plus a command,
    gated by the leg's `allowed` regexes).

That is the "compute, inference, memory and schedule are interchangeable units addressed
by id" claim in CLAUDE.md, tested as one system instead of one clause at a time.

## Which model is `brain`

`brain` rides the SAME plug as the builder (`BACKEND`). `_ai_meta(backend=...)` lets one
run name a different plug for `brain` — a strong builder wiring a WEAK AI agent is the
realistic shape: the builder must reach that agent by id and tolerate what it returns.

## Why the AI-agent requirement is stated, not left to be discovered

The stock task MENTIONS `brain` but requires nothing of it, and opus's earlier pass never
called it — so "did it wire inference" was untested rather than answered. Withholding the
requirement here would repeat that: a failure would be uninterpretable (did it not manage
to, or not think to?). The task says the specialist must consult that agent; it does NOT
say for what, with which verb, or how to handle the reply. Same discipline as probe 06's
graph-shape paragraph.

## Held constant

Everything else. Fixture, readings, canary, clock tick, unattended window, idle policy and
`score` all come from `live_subai` — imported, not copied.
"""

from __future__ import annotations

import asyncio
import sys
from pathlib import Path

# e2e bootstrap — walk up to `.e2e-root`; no hard-coded depth, so moving this
# file into a folder can never silently break its imports again.
sys.path.insert(
    0,
    str(
        next(d for d in Path(__file__).resolve().parents if (d / ".e2e-root").exists())
    ),
)
from helpers import (
    verdict,
)

_ARGV = sys.argv[:]
sys.argv = [sys.argv[0]]

from tests.agentic_advanced.E30_subai import probe as P

# `brain` rides the SAME backend as the builder — the one the run's connection env
# wires (helpers read it; nothing here names a host) — never a hardcoded local default,
# which would reach for a server nobody wired.
BRAIN_BACKEND = P.BACKEND

EXTRA = (
    "\n\nThree more constraints on HOW you build it. "
    "First: express the program's logic as a STATE GRAPH — named node functions that "
    "each take the state and return an update, an explicit edge map naming which node "
    "follows which, and a small runner that walks it. Write that runner yourself: this "
    "kernel has no graph library installed and you must not import one. "
    "Second: your new AI agent must ALSO consult `brain` — the kernel's own inference "
    "unit, a separate model stream, NOT you the builder — as part of each tick, "
    "reaching it by id like any other agent; inference is a unit you wire. What it "
    "asks is your call. It is a small model, so do not depend on it being clever. "
    "Third: keep the result in the durable store as before, because I am still going to "
    "walk away.\n\n"
    "I have already left — this is a one-way instruction and there is nobody to answer "
    "a question. If something is ambiguous or looks wrong, decide it yourself, write "
    "down the call you made, and finish building."
)


# NO GRAPH LIBRARY. Installing one (e.g. `langgraph`) would make the probe depend on a
# package to test something that is not about the package — whether a familiar
# DECOMPOSITION (nodes, an edge map, a runner) helps — and would change the interpreter
# as well as the ask, so a failure would be attributable to either.


async def main() -> None:
    # SAME fixture and SAME interpreter as probe 05 — only the ask differs.
    ok = await P.run(
        task=P.TASK + EXTRA,
        brain_backend=BRAIN_BACKEND,
        banner="COMBINED — state graph + child workers + clock + an AI agent",
    )
    print(f"\n  COMBINED  ->  {'PASS' if ok else 'FAIL'}")
    verdict(ok)
    print(
        "  A pass means one system spanning compute (exec_bridge), inference\n"
        "  (the AI agent, by id), memory (the store) and time (the clock) — all wired by\n"
        "  the model. A fail is only interesting WHICH of those it dropped."
    )
    sys.exit(0 if ok else 1)


if __name__ == "__main__":
    asyncio.run(main())
