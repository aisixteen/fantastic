# `user_actions` — proposed, clicked twice, bought once — user actions are idempotent

Two properties pulling in opposite directions: the model must name an
execution it will never run itself, and the SUBSTRATE must honour that
name for a caller who is not the model. The AI holds no grant on the
desk — that it cannot buy is half the measurement.

## The scenario is DATA

`scenario.yaml` holds the task, the roster, the grants and the assertions. It is
the ONLY copy: the same file runs against all four kernels, so changing the task
moves every runtime at once. That is the only thing that makes four results
comparable — copies drift, and drifted results measure different things while
still looking like one table.

## Roles, not bundle names

The roster names a ROLE; each runner supplies whatever its kernel has:

| role | python | rust | swift |
|---|---|---|---|
| `memory` | `file_bridge` data store | `file_bridge` data store | `file_bridge` data store |
| `purchase_desk` | `purchase_probe` (test-only) | `purchase_probe` (test-only) | `purchase_probe` (test-only) |
| `render_sink` | `cli` | `cli` bundle | `cli` bundle |

`purchase_probe` is a TEST-ONLY bundle on every runtime (a rust feature `scenario-probes`, a
swift define `SCENARIO_PROBES`, a `PYTHONPATH` entry for python); a default build
carries none of it.

## Running it

```sh
cd src/lib/python && KERNEL=python uv run python ../../../e2e/runners/host.py user_actions
cd src/lib/python && KERNEL=rust  uv run python ../../../e2e/runners/host.py user_actions
cd src/lib/python && KERNEL=swift uv run python ../../../e2e/runners/host.py user_actions
```

…or the whole cross-product through the launcher:

```sh
cd src/lib/python && uv run python ../../../e2e/run.py --group scenario
```
