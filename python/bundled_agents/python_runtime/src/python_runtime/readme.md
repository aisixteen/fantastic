# python_runtime — subprocess Python exec
Verb: exec (`code`, `timeout`). Each call is a fresh subprocess (stateless). Per-agent interrupt/stop.

## Cross-runtime interpreter determinism
On first `boot`, when neither `record.python` nor `record.venv` is set, this bundle persists `sys.executable` into `record.python` via `kernel.update`. The interpreter is then pinned on disk — opening the same workdir under a different runtime (e.g. the Rust kernel's `python_runtime`, which lacks `sys.executable` and would otherwise fall back to `which python3`) reads the persisted path and dispatches to the same interpreter the Python kernel originally used. The behaviour is idempotent: subsequent boots are no-ops when `python` is already set, and explicit `python` / `venv` overrides on the record always take precedence.
