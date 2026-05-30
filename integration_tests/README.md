# Fantastic integration tests

Cross-runtime tests that spawn real `fantastic` kernels as subprocesses,
pair them via the `kernel_bridge` bundle, and exercise the canonical
verb surface (send / reflect / streams) across the wire.

These are **integration** tests — heavyweight, slow, real network I/O.
They live outside the per-runtime unit-test trees on purpose. Each
test launches one or more `fantastic` processes, waits for them to
bind their HTTP/WS surfaces, configures bridges, and verifies the
wire-level interop end-to-end.

**Python is the orchestrator** because it's the fastest iteration loop
(no recompile per change). The kernels under test can be any
combination of `python/.venv/bin/fantastic` and
`swift/.build/{debug,release}/fantastic`.

## Layout

```
integration_tests/
  pyproject.toml      uv-managed; pytest + websockets + httpx
  conftest.py         shared fixtures (free_port, parity_tmp, python/swift kernels)
  helpers/
    kernel_proc.py    subprocess wrapper for fantastic kernels
    seeding.py        one-shot CLI seeding (web / web_ws / bridge_ws)
    ws.py             minimal WS client: ws_call, ws_emit, ws_session
  test_bridge_*.py    cross-runtime bridge tests (WS-only)
  tmp/                per-run scratch workdirs (gitignored)
```

## Setup

```bash
cd integration_tests
uv sync
```

Path discovery for the kernel binaries is automatic; the conftest
looks for:

  Python kernel: `python/.venv/bin/fantastic` (run `cd ../python &&
  uv sync` once to install)

  Swift kernel:  `swift/.build/debug/fantastic` (run `cd ../swift &&
  swift build` to compile)

A test is skipped (not failed) when its required kernel binary
isn't built yet.

## Running

```bash
cd integration_tests
uv run pytest                                  # everything
uv run pytest test_bridge_python_python_ws.py  # one file
uv run pytest -k swift                          # filter by name
uv run pytest -s                               # streamed stdout (verbose)
```

## What's tested

All bridge transport is **WS-only, asymmetric**: the client bridge
opens a WS to the server's `web_ws` surface and ships raw
`{type:"call", target, payload}` frames — no peer bridge on the
server side. The server must be up before the client boots (the WS
connects eagerly), so tests spawn the server kernel first.

| test | runtimes paired | verb surface |
|---|---|---|
| `test_bridge_python_python_ws.py` | Python ↔ Python | `forward(reflect)` + `forward(list_agents)` |
| `test_bridge_swift_python_ws.py`  | Swift → Python  | `forward(reflect)` (Swift client, Python server) |
| `test_bridge_python_swift_ws.py`  | Python → Swift  | `forward(reflect)` (Python client, Swift server) |
| `test_bridge_stream_python_python_ws.py` | Python ↔ Python | `watch_remote` → `event` re-emit streaming |

Each test runs against fresh workdirs under `tmp/<test-name>/<uuid>/`.
On failure the workdir is preserved for inspection (look for `agent.json`
state + `lock.json` to see what the kernel had).

## Adding a test

1. Drop a `test_*.py` in this directory.
2. Use the `python_kernel` / `swift_kernel` fixtures to spawn instances
   (spawn the **server** first — the client bridge connects eagerly).
3. Seed with `helpers.seeding`: `seed_web` + `seed_web_ws` (Python
   server) and `seed_bridge_ws(..., peer_id="core", peer_port=<server>)`
   on the client. Then call `kernel.call("bridge", "boot")` once as an
   idempotent connect guard.
4. Make assertions about the wire shape; on drift, the test fails
   loudly with the divergent JSON included.

Drift caught by these tests is **definitionally a bug in the non-Python
runtime** — Python is the canonical reference for the Fantastic protocol.

## Why not in `python/tests` or `swift/Tests`?

- `python/tests/` are in-process unit tests against the Python kernel
  alone — no subprocess, no cross-runtime.
- `swift/Tests/` are in-process unit tests against the Swift kernel
  alone — no subprocess, no cross-runtime.
- These are different: they exercise the **interop surface between
  kernels**, which lives in neither tree.
