# `task_registry` (E135) — which agent or subtree holds which task, driven by a scripted model

Free: no model. A local OpenAI-compatible server (`e2e/mocks/vllm.py`) answers from a
fixed script; the agents are the kernel's own `vllm_backend` agents (ai_core) on the
PACKAGED host of `RUNTIME`, driven over `web_rest`. Both registries are OFF by default:
the probe turns them on with `FANTASTIC_AGENTIC_RESOURCES=1` and
`FANTASTIC_TASK_REGISTRY=1`. The task registry keeps its tables in a `ram_store` agent
named by its record (`task_store_id`); the wiring is explicit on both ends.

The script: a lead `L0` (spawned by the test through `agentic_resources`, with the write
verbs of the task registry in its card) adds a top task, spawns a sub-lead `SA` and a
worker `WA`, adds a sub-task, `assign`s it to `SA`, `reassign`s it to `SA`'s subtree,
and on `WA`'s behalf records the worker's `report` and the task `status`. `WA` only
calls `agentic_resources` `report`; its own try at the task registry is refused by the
call gate (it holds no grant).

Checks: `list` and `get` show the tree, the holder history and the last report; the
store's `dump` holds one row per write under the registry's key; the worker's direct
write left nothing behind. The checks are listed in `probe.py`'s docstring.

NOT part of any CI run: coded, run by hand.

```sh
RUNTIME=python uv run python e2e/tests/kernel/E135_task_registry/probe.py   # from src/lib/python
RUNTIME=rust   uv run python e2e/tests/kernel/E135_task_registry/probe.py
```
