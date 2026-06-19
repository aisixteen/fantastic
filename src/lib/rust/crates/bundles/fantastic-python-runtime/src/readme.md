# python_runtime — subprocess Python exec

Each `exec` is its own process: `<interp> -c <code>`. Stateless across
calls (no shared globals or KV cache). Per-agent in-flight tracking
enables `interrupt` (SIGINT) and `stop` (SIGKILL).

**Interpreter resolution** (highest priority first):

1. `payload.python` — explicit interpreter path on the call
2. `payload.venv` → `<venv>/bin/python` (or `bin/python3` / `Scripts/python.exe`)
3. `record.python` — explicit interpreter on the agent record
4. `record.venv` → same venv-dir lookup as #2
5. `FANTASTIC_PYTHON` env var — Rust-runtime default
6. `which python3` — POSIX PATH lookup
7. `which python` — fallback
8. Error: `{"error": "python_runtime: no Python interpreter resolved; set record.python or FANTASTIC_PYTHON"}`

The Rust kernel has no `sys.executable` equivalent, so the resolution
ladder extends the Python bundle's with `FANTASTIC_PYTHON` + PATH
discovery. For deterministic cross-runtime behaviour, set
`record.python` once on agent boot (the Python bundle's `_boot` does
this automatically via `sys.executable`); subsequent reads under the
Rust kernel hit the same path on disk.

**Verbs**:

| verb       | args                                                 | reply                                          |
|------------|------------------------------------------------------|------------------------------------------------|
| `reflect`  | _none_                                               | `{id, sentence, cwd, python, venv, in_flight, verbs}` |
| `exec`     | `code:str`, `timeout:float?` (60), `cwd?`, `python?`, `venv?` | `{stdout, stderr, exit_code, timed_out}` |
| `interrupt`| _none_                                               | `{interrupted: int}` (SIGINT to in-flight)     |
| `stop`     | _none_                                               | `{killed: int}` (SIGKILL to in-flight)         |
| `boot`     | _none_                                               | `null` (no-op)                                 |
