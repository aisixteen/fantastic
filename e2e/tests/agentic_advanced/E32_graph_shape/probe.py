"""LIVE TEST 07 — GRAPH SHAPE, an A/B: does a FAMILIAR DECOMPOSITION make a weak
model succeed? (Named after the state-graph SHAPE it asks for. No graph library
exists anywhere in this codebase — see "Why no graph library".)

    cd src/lib/python && ARM=bare  uv run python -u ../../../tmp/live_graph_shape.py
    cd src/lib/python && ARM=graph uv run python -u ../../../tmp/live_graph_shape.py

## The question

gemma-4-12B failed probe 05 six times (2026-08-06) and never on reasoning: right agents,
right verbs, 4/4 planning language, every run. It lost on OUR contract. It has also seen
thousands of state-graph examples in training and zero fantastic readmes.

So: is the blocker a SHAPE it has never seen, or capability?

  * passes `graph` and fails `bare` ⇒ the shape carries it. Actionable: write the readmes
    to look like shapes models have already seen, and the emergence thesis survives with
    a much better prompt surface.
  * fails BOTH  ⇒ the shape is not the blocker; framing the work as a graph would not
    have saved it.
  * passes BOTH ⇒ the probe-05 failures were about something probe 04's task does not
    exercise (sub-agent structure, scheduling), not authoring.

A single arm answers none of those, which is why this is an A/B and not a probe.

## What is held constant

EVERYTHING except the ask. The fixture, the readings, the scoring, the canary, the idle
policy AND the interpreter come from `live_build.run()` — imported, not copied, because
two copies of a scoring path let the arms diverge silently and the emergence README
already records that failure twice.

ONE variable: the task carries one extra paragraph asking for the logic as a state graph
— named node functions, an explicit edge map, and a runner the model writes itself.

## Why no graph library

An earlier version installed `langgraph` into a side venv, pointed the executor at THAT
interpreter, and mounted the package source as an agent so the API could be read. That
made the probe depend on a package to answer a question that is not about the package,
and it broke the single variable: the two arms then differed in the ask AND in the
interpreter, so a FAIL was attributable to either. The thing being measured is whether a
familiar DECOMPOSITION helps — nodes, edges, a runner — not whether the model can recall
one library's API. So the model writes the runner, and both arms run identically.
"""

from __future__ import annotations

import asyncio
import os
import sys
from pathlib import Path

# e2e bootstrap — walk up to `.e2e-root`; no hard-coded depth, so moving this
# file into a folder can never silently break its imports again.
sys.path.insert(0, str(next(d for d in Path(__file__).resolve().parents
                            if (d / ".e2e-root").exists())))
from helpers import (
    verdict,
)

_ARGV = sys.argv[:]
sys.argv = [sys.argv[0]]

from tests.agentic.E20_build import probe as P  # noqa: E402

ARM = os.environ.get("ARM", "bare").strip().lower()
# The operator's venv. NOT discovered and not guessed: which interpreter may run is the
# operator's decision (`exec_bridge` refuses per-call overrides for exactly that reason),
# so a fixture supplies it the way an operator would.

# Appended to probe 04's task, unchanged otherwise. It asks for a SHAPE only; it
# does NOT say how to wire a graph, which executor to use, or what to store —
# those stay discoverable, because dictating them would make a pass meaningless.
GRAPH_ASK = (
    " Structure the totalling logic as a STATE GRAPH rather than as a straight-line "
    "script: named node functions that each take the state dict and return an update, "
    "an explicit edge map naming which node follows which, and a small runner that "
    "walks it from an entry node to the end. Write that runner yourself — this kernel "
    "has no graph library installed and you must not import one."
)


# NO LANGGRAPH DEPENDENCY. The arm used to install `langgraph` into a side venv,
# point the executor at that interpreter, and mount the package source as an agent so
# the model could read the real API. That made the probe need a package to answer a
# question that is not about the package: the variable is whether a FAMILIAR SHAPE —
# nodes, an edge map, a runner — helps a model that has seen thousands of such graphs
# and zero of our readmes. The shape is what is being tested, so the model writes the
# runner itself and the arm runs on the same interpreter as the control.
#
# That also restores the single variable. With a different interpreter the two arms
# differed in TWO ways, and a FAIL could be attributed to either.


async def main() -> None:
    if ARM not in ("bare", "graph"):
        sys.exit(f"ARM must be 'bare' or 'graph', got {ARM!r}")

    if ARM == "bare":
        # Byte-identical to probe 04. It is the CONTROL, and it must not drift from it.
        ok = await P.run(banner="A/B arm 'bare' — author a plain program")
    else:
        # SAME fixture, SAME interpreter as the control — only the ask differs.
        ok = await P.run(
            task=P.TASK + GRAPH_ASK,
            banner="A/B arm 'graph' — author the same logic as a state graph",
        )

    print(f"\n  ARM={ARM}  ->  {'PASS' if ok else 'FAIL'}")
    verdict(ok)
    print(
        "  Interpretation needs BOTH arms: graph-pass + bare-fail ⇒ a familiar SHAPE "
        "carries it;\n  both fail ⇒ the shape is not the blocker; both pass ⇒ the "
        "probe-05 blocker\n  was structure/scheduling, not authoring."
    )
    sys.exit(0 if ok else 1)


if __name__ == "__main__":
    asyncio.run(main())
