# `compose` — composed + computed + stored — the answer cannot come from weights

The answer CANNOT come from the model's weights: the digest is of a nonce
minted at test time, so the only way to produce it is to actually run code.
Fabrication is not merely detected here, it is impossible.

## The scenario is DATA

`scenario.yaml` holds the task, the roster, the grants and the assertions. It is
the ONLY copy: the same file runs against all four kernels, so changing the task
moves every runtime at once. That is the only thing that makes four results
comparable — copies drift, and drifted results measure different things while
still looking like one table.

## Roles, not bundle names

The roster names a ROLE; each runner supplies whatever its kernel has:

| role | python | rust |
|---|---|---|
| `memory` | `file_bridge` data store | `file_bridge` data store |
| `compute` | `exec_bridge` | `exec_bridge` |
| `render_sink` | `cli` | `cli` bundle |

## Running it

```sh
cd src/lib/python && KERNEL=python uv run python ../../../e2e/runners/host.py compose
cd src/lib/python && KERNEL=rust  uv run python ../../../e2e/runners/host.py compose
```

…or the whole cross-product through the launcher:

```sh
cd src/lib/python && uv run python ../../../e2e/run.py --group scenario
```
