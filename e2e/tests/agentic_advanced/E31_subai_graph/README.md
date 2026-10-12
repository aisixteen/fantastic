# `subai_graph` — the same build, scored as a GRAPH — edges, not just nodes

LIVE TEST 08 — SUBAI + GRAPH SHAPE: the combined composition — the sub-agent
task (probe 06) with the state-graph decomposition ask (probe 07's variable).
No graph library exists anywhere in this codebase; the SHAPE is the variable.

    cd src/lib/python && uv run python -u ../../../e2e/run.py --group agentic_advanced --id subai_graph --lanes 1

## What this adds over probes 05 and 06

Probe 06 asked for one program on `exec_bridge`. Probe 05 asked for a specialist with
CHILD workers wired to a scheduler. Neither asked for all of it at once, and neither put
an AI agent inside the thing being built. Measured 2026-08-07: opus passed 06 (both arms)
and passed 05, but 05 ran before `kernel_state._EXECUTORS` named `exec_bridge`, so it
authored on `python_runtime`. So THIS combination has never run:

  * the totalling logic as a state graph — nodes, edge map, self-written runner,
  * inside a SPECIALIST that owns child workers (probe 05's shape),
  * wired to the `clock` scheduler so it re-checks unattended (probe 05's clock),
  * consulting the `brain` AI agent by id (new here — inference as a WIRED UNIT),
  * on `exec_bridge` with an interpreter allowlist (the current executor).

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

## Files

| file | what it is |
|---|---|
| `probe.py` | the runner — builds the tree, drives the model, scores it |
| `kernel.yaml` | the agent roster this test builds, declared |
| `test.yaml` | limits and knobs; **backend-agnostic by construction** |

Run it alone:

```sh
cd src/lib/python && uv run python -u ../../../e2e/tests/agentic_advanced/subai_graph/probe.py
```

…or through the launcher, which picks the backend and the lanes:

```sh
cd src/lib/python && uv run python ../../../e2e/run.py --id subai_graph
```
