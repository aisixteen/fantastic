# `ledger` — the double-fire trap — one coin, proven twice

The double-fire trap. Without a named execution the repeat dispenses a
second coin; with an `id` the repeat returns the FIRST run's recorded
outcome. The kernel's ensure-semantics are not in doubt — the model's use
of them is what is measured.

## The scenario is DATA

`scenario.yaml` holds the task, the roster, the grants and the assertions. It is
the ONLY copy: the same file runs against all four kernels, so changing the task
moves every runtime at once. That is the only thing that makes four results
comparable — copies drift, and drifted results measure different things while
still looking like one table.

## Roles, not bundle names

The roster names a ROLE; each runner supplies whatever its kernel has:

| role | python | js | rust | swift |
|---|---|---|---|---|
| `memory` | `text_file` bundle | in-process handler | pending text_file port | pending text_file port |
| `dispenser` | in-process handler | in-process handler | **none — skipped** | **none — skipped** |
| `render_sink` | `cli` | — | `cli` bundle | `cli` bundle |

**Skipped on rust and swift**, by name: a compiled kernel runs only bundles
compiled INTO it, and no shipped bundle plays `dispenser` — a stateful fixture
with bespoke verbs. The runner refuses up front rather than approximating it
with something that behaves differently, because a number that looks
comparable and is not is worse than an honest skip.

## Running it

```sh
cd src/lib/python && KERNEL=python uv run python ../../../e2e/runners/host.py ledger
cd e2e && node runners/inproc_js.mjs ledger
cd src/lib/python && KERNEL=rust  uv run python ../../../e2e/runners/host.py ledger
cd src/lib/python && KERNEL=swift uv run python ../../../e2e/runners/host.py ledger
```

…or the whole cross-product through the launcher:

```sh
cd src/lib/python && uv run python ../../../e2e/run.py --group scenario
```
