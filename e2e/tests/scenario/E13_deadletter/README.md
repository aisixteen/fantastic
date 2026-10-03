# `deadletter` — a flaky call is retried by the kernel and mourned by the ledger

Whether the model lets the KERNEL retry — one send carrying a `retries`
budget — rather than re-sending by hand. Re-sending by hand produces the
right answer for the pump and no dead-letter row for the valve, which is
exactly the state a caller cannot reason about later.

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
| `flaky_plant` | `plant_probe` (test-only) | `plant_probe` (test-only) | `plant_probe` (test-only) |
| `render_sink` | `cli` | `cli` bundle | `cli` bundle |

`plant_probe` is a TEST-ONLY bundle on every runtime (a rust feature `scenario-probes`, a
swift define `SCENARIO_PROBES`, the `e2e/probes/scenario_probes.py` launcher that registers it
for python); a default build
carries none of it.

## Running it

```sh
cd src/lib/python && KERNEL=python uv run python ../../../e2e/runners/host.py deadletter
cd src/lib/python && KERNEL=rust  uv run python ../../../e2e/runners/host.py deadletter
cd src/lib/python && KERNEL=swift uv run python ../../../e2e/runners/host.py deadletter
```

…or the whole cross-product through the launcher:

```sh
cd src/lib/python && uv run python ../../../e2e/run.py --group scenario
```
