# `sqlite_history_restart` (E136) — an agent's history outlives the kernel process

Free: no model. A local OpenAI-compatible server (`e2e/mocks/vllm.py`) answers from a
fixed script; the agent is the kernel's own `vllm_backend` agent (ai_core) on the
PACKAGED host of `RUNTIME` (python | rust), driven over `web_rest`.

The agent's `memory_store_id` names a `sqlite_store` agent whose file is the record's
`path` (`stores/hist.db`, config, never hardcoded). One scripted turn is run, the kernel
process is stopped (the stop folds the WAL into the file) and a NEW process is booted on
the same directory. Nothing is dumped or injected: that is what the durable store is
for (E78/E83/E87 carry a `ram_store` across with `dump` and `inject` and spend real
turns).

Checks: the store holds the same rows (same ids, same content) under the same
`<guid>|<agent>` key after the restart; a second message reaches the model with the
first turn in its request; the new rows get ids above the old maximum; the file is on
disk and the store's `reflect` says durable and names the record's `path`. The checks
are listed in `probe.py`'s docstring.

NOT part of any CI run: coded, run by hand.

```sh
RUNTIME=python uv run python e2e/tests/kernel/E136_sqlite_history_restart/probe.py   # from src/lib/python
RUNTIME=rust   uv run python e2e/tests/kernel/E136_sqlite_history_restart/probe.py
```
