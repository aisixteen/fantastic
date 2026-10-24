# `dialog` — it DISCOVERS an unnamed peer from the tree and really acts on it

The simplest real test of self-description: the task names NO agent and NO
verbs, so the model has to reflect the tree to find the store and learn how
to write to it. Scored by reading the store back — a model that says ANSWER
STORED without calling anything cannot pass.

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
| `render_sink` | `cli` | `cli` bundle |

## Running it

```sh
cd src/lib/python && KERNEL=python uv run python ../../../e2e/runners/host.py dialog
cd src/lib/python && KERNEL=rust  uv run python ../../../e2e/runners/host.py dialog
```

…or the whole cross-product through the launcher:

```sh
cd src/lib/python && uv run python ../../../e2e/run.py --group scenario
```
