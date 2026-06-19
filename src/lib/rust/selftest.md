# Fantastic selftests (Rust)

Every crate owns a hand-written, executable test spec. AI agents
(Claude Code, etc.) read these files, ask required pre-flight questions,
drive the system at the user-facing surface, and fill summary tables.

Complement to `cargo test` — `cargo test` covers in-process unit logic;
selftests cover the user surface (CLI, HTTP, WS, browser).

## For Claude Code / other AI agents

1. Read this index completely.
2. Parse user intent → set of scopes to **INCLUDE / EXCLUDE**.
3. From the index table, pick files whose scopes are all allowed.
4. For each selected file, in order:
   - Read it end-to-end.
   - If `requires:` lists pre-flight items, **ASK USER** for missing
     info (running daemon, free port, etc.) **BEFORE** wiping any state.
   - Run tests in numerical order.
   - Capture pass/fail + notes per test.
5. After all files: emit ONE aggregate report — rows (file, test#),
   columns (name, pass/fail, notes). List skipped files with reason.

Strict rules:

- Never silently fake a skip.
- Never run destructive ops without user confirmation if state exists.
- If a regression signal in a test triggers, **STOP** that file's
  remaining tests and flag it.
- Do not invent expected output — ask if the spec is unclear.
- A correctly-deployed `fantastic` answers `kernel.reflect` over WS
  with the agent tree, the bundle catalog (`bundles=all`), and the root
  readme (`readme=true`) — the readme carries the transport/wire docs
  you need to issue your first send. Open `ws://host/<any-agent>/ws` and
  send `{"type":"call","target":"kernel","payload":{"type":"reflect",
  "readme":true},"id":"1"}`. If you find yourself reading
  `crates/fantastic-kernel/src/` or `crates/bundles/fantastic-web/src/`
  to figure out a transport URL — that belongs in the root readme. Stop
  and flag it.

## Stateful crates need a running `fantastic` daemon

Some crates hold state in process-memory that doesn't survive
separate one-shot CLI invocations:

- `fantastic-web` — the axum server instance.
- `fantastic-web-ws` — active WebSocket connections + per-client
  inbox queues.

Their selftests start a single `fantastic` and drive it over the WS
proxy (`ws://localhost:$PORT/<id>/ws`). Each selftest's pre-flight
defines a shell `call()` helper that wraps a one-shot WS round-trip
in inline Python (or any WS client of choice). Don't try to use the
`call` subcommand for these — one-shots spawn a fresh process and
can't see live in-memory state.

## Index

The Rust runtime ships the same 21 bundles as Python and serves the
same user-facing wire surfaces (CLI, HTTP, WS, REST, PTY, browser).
Two kinds of specs apply:

### Cross-runtime — drive Python's per-bundle specs against `./target/release/fantastic`

The Python `selftest.md` index ([`../python/selftest.md`](../python/selftest.md))
points at 19 per-bundle specs. They describe user-facing behaviour
(verb shapes, persisted file layout, WS frames). Run them against
the Rust binary by substituting the binary path:

```bash
# Build the Rust binary once:
cd rust && cargo build --release --bin fantastic

# Then run Python selftests as written, but with the Rust binary:
export PATH="$(pwd)/target/release:$PATH"
which fantastic    # → /<your-repo>/rust/target/release/fantastic
fantastic --version 2>&1 | head -1

# Drive each Python per-bundle spec as written.
cd ../python
cat bundled_agents/file/selftest.md     # ← read; run the bash blocks
cat bundled_agents/web/host/selftest.md
# ...etc, for every bundle you want to exercise
```

Specs that work cross-runtime without modification: file, web, web_ws,
web_rest, ollama_backend (needs running ollama),
nvidia_nim_backend (needs api_key), scheduler,
kernel_bridge (WS-only, asymmetric; memory + WS + SSH+WS transports), local_runner, ssh_runner.

View/webapp specs now live in the decoupled frontend kernel — see
[`../ts/`](../ts/); this host serves it generically via a `file` agent.

Specs with Rust-specific deltas worth noting (verbs match; only the
binary path + a few env vars differ): python_runtime
(uses `FANTASTIC_PYTHON` env on Rust — see overlay spec), terminal_backend
(image-paste via WS binary frame channel, also covered in overlay).

### Rust-specific overlay specs

These cover behaviour that exists ONLY in the Rust runtime, has no
Python equivalent, or differs in ways the cross-runtime specs don't
exercise:

| File | Scopes | Requires |
|---|---|---|
| [`selftest/feature_gates.md`](selftest/feature_gates.md) | build, packaging | `cargo` |
| [`selftest/python_runtime_resolution.md`](selftest/python_runtime_resolution.md) | python_runtime, env, PATH | Rust `fantastic` binary, optional Python on PATH |
| [`selftest/binary_frame_chunking.md`](selftest/binary_frame_chunking.md) | WS, transport, multi-modal | Rust `fantastic` binary, websocket client |
| [`selftest/cross_runtime_workdir.md`](selftest/cross_runtime_workdir.md) | persistence, cross-runtime | Both Python `uv sync` and Rust binary |

These four specs run only against the Rust runtime — they assert
things the Python kernel doesn't expose (the `FANTASTIC_PYTHON` env,
the chunked WS binary frame protocol, the feature-gate compile
matrix, and round-trip workdir loading across both kernels).
