# `graph_shape` — A/B — does naming the graph change the shape it builds?

LIVE TEST 07 — GRAPH SHAPE, an A/B: does a FAMILIAR DECOMPOSITION make a weak
model succeed? (Named after the state-graph SHAPE it asks for. No graph library
exists anywhere in this codebase — see "Why no graph library".)

    cd src/lib/python && ARM=bare  uv run python -u ../../../tmp/live_graph_shape.py
    cd src/lib/python && ARM=graph uv run python -u ../../../tmp/live_graph_shape.py

## The question

A small model can fail probe 05 without failing on reasoning: right agents, right verbs,
planning language — and still lose on OUR contract. It has seen thousands of state-graph
examples in training and zero fantastic readmes.

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

## Files

| file | what it is |
|---|---|
| `probe.py` | the runner — builds the tree, drives the model, scores it |
| `kernel.yaml` | the agent roster this test builds, declared |
| `test.yaml` | limits and knobs; **backend-agnostic by construction** |

Run it alone:

```sh
cd src/lib/python && uv run python -u ../../../e2e/tests/agentic_advanced/graph_shape/probe.py
```

…or through the launcher, which picks the backend and the lanes:

```sh
cd src/lib/python && uv run python ../../../e2e/run.py --id graph_shape
```
