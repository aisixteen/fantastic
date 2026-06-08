# Fantastic selftests (Swift)

Every bundle owns a hand-written, executable test spec. AI agents
(Claude Code, etc.) read these files, ask required pre-flight questions,
drive the system at the user-facing surface, and fill summary tables.

Complement to `swift test` — `swift test` covers in-process unit logic;
selftests cover the user surface (CLI, HTTP, WS, browser).

## For Claude Code / other AI agents

1. Read this index completely.
2. Parse user intent → set of scopes to **INCLUDE / EXCLUDE**.
3. From the index table, pick files whose scopes are all allowed.
4. For each selected file, in order:
   - Read it end-to-end.
   - If `requires:` lists pre-flight items, **ASK USER** for missing
     info (running daemon, free port, on-device model, etc.) **BEFORE**
     wiping any state.
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
- A correctly-deployed `fantastic` answers `reflect` over the CLI
  (`fantastic reflect core`) with the agent tree, and over WS with the
  same plus the root readme (`readme=true`) — the readme carries the
  transport/wire docs you need to issue your first send. Open
  `ws://host/<any-agent>/ws` and send `{"type":"call","target":"core",
  "payload":{"type":"reflect","readme":true},"id":"1"}`. If you find
  yourself reading `Sources/FantasticKernel/` or
  `Sources/FantasticWeb/` to figure out a transport URL — that belongs
  in the root readme. Stop and flag it.

The Swift root agent id is **`core`** (not `fs_loader`, not `kernel`) —
the native runtime keeps the userland-orchestrator root id. Substitute
`core` anywhere a cross-runtime Python spec writes `fs_loader` as the
create/list target.

## Stateful bundles need a running `fantastic` daemon

Some bundles hold state in process-memory that doesn't survive
separate one-shot CLI invocations:

- `web` — the HTTP server instance.
- `web_ws` — active WebSocket connections + per-client inbox queues.

Their selftests start a single `fantastic` and drive it over the WS
proxy (`ws://localhost:$PORT/<id>/ws`). Each selftest's pre-flight
defines a shell `call()` helper that wraps a one-shot WS round-trip in
inline Python (or any WS client of choice). Don't try to use the
one-shot `fantastic <id> <verb>` form for these — one-shots spawn a
fresh process and can't see live in-memory state.

## Index

The Swift runtime ships the same bundle set as Python and serves the
same user-facing wire surfaces (CLI, HTTP, WS, REST, PTY, browser).
Two kinds of specs apply.

### Cross-runtime — drive Python's per-bundle specs against the Swift binary

The Python `selftest.md` index
([`../python/selftest.md`](../python/selftest.md)) points at the
per-bundle specs under
[`../python/bundled_agents/**/selftest.md`](../python/bundled_agents/).
They describe user-facing behaviour (verb shapes, persisted file
layout, WS frames) for **wire-identical** bundles. Run them against the
Swift binary by substituting the binary path and the root id:

```bash
# The Swift binary (debug build; root agent id is `core`):
BIN=/Users/oleksandr/Projects/fantastic_canvas/swift/.build/debug/fantastic
export PATH="$(dirname "$BIN"):$PATH"
which fantastic    # → .../swift/.build/debug/fantastic

# Drive each Python per-bundle spec as written, but:
#   • point `fantastic` at the Swift binary (above)
#   • replace the create/list root target `fs_loader` → `core`
cat ../python/bundled_agents/file/selftest.md       # ← read; run the bash blocks
cat ../python/bundled_agents/web/host/selftest.md
# ...etc, for every bundle you want to exercise
```

Surviving bundles that run cross-runtime against the Swift binary
(verbs, on-disk layout, and WS frames are wire-identical):

| bundle | Python spec |
|---|---|
| file | [`file/selftest.md`](../python/bundled_agents/file/selftest.md) |
| yaml_state | [`yaml_state/selftest.md`](../python/bundled_agents/yaml_state/selftest.md) |
| proxy_agent | covered by the host WS specs (`web_ws`) + native overlays |
| tools | covered by `reflect` across the per-bundle specs |
| scheduler | [`scheduler/selftest.md`](../python/bundled_agents/scheduler/selftest.md) |
| cli | [`cli/selftest.md`](../python/bundled_agents/cli/selftest.md) |
| kernel_bridge | [`kernel_bridge/selftest.md`](../python/bundled_agents/bridge/kernel_bridge/selftest.md) |
| web | [`web/host/selftest.md`](../python/bundled_agents/web/host/selftest.md) |
| web_ws | [`web/web_ws/selftest.md`](../python/bundled_agents/web/web_ws/selftest.md) |
| web_rest | [`web/web_rest/selftest.md`](../python/bundled_agents/web/web_rest/selftest.md) |
| ollama_backend | [`ai/ollama/ollama_backend/selftest.md`](../python/bundled_agents/ai/ollama/ollama_backend/selftest.md) (needs running ollama) |
| nvidia_nim_backend | [`ai/nvidia/nvidia_nim_backend/selftest.md`](../python/bundled_agents/ai/nvidia/nvidia_nim_backend/selftest.md) (needs `NVAPI_KEY`) |
| foundation_models_backend | native overlay (no Python equivalent) — see below |
| local_runner | [`runner/local_runner/selftest.md`](../python/bundled_agents/runner/local_runner/selftest.md) (Full tier only) |
| python_runtime | [`python_runtime/selftest.md`](../python/bundled_agents/python_runtime/selftest.md) (Full tier only) |
| ssh_runner | [`runner/ssh_runner/selftest.md`](../python/bundled_agents/runner/ssh_runner/selftest.md) (Full tier only) |
| terminal_backend | [`terminal/selftest.md`](../python/bundled_agents/terminal/selftest.md) (Full tier only) |

`proxy_agent` and `tools` have no standalone Python spec — they are
exercised structurally by `reflect` and the host WS specs.

View/webapp specs live in the decoupled frontend kernel — see
[`../ts/`](../ts/); this host serves it generically via a `file` agent.

The four subprocess bundles (terminal_backend, python_runtime,
local_runner, ssh_runner) exist **only in the Full (macOS) tier** —
the `.build/debug/fantastic` binary IS the Full tier, so their
cross-runtime specs run as written. Under the Embedded tier they are
compile-time excluded; see `selftest/feature_gates.md`.

### Swift-specific overlay specs

These cover behaviour that exists ONLY in the Swift runtime or has no
Python equivalent:

| File | Scopes | Requires |
|---|---|---|
| [`selftest/foundation_models_backend.md`](selftest/foundation_models_backend.md) | ai, apple | macOS 26 + Apple Silicon (FoundationModels); structural-only otherwise |
| [`selftest/feature_gates.md`](selftest/feature_gates.md) | build, packaging | `swift` toolchain + Xcode iPhoneSimulator SDK |

`foundation_models_backend` wraps Apple's on-device `FoundationModels`
framework behind the same LLM-backend verb surface as ollama/nvidia —
no Python equivalent. `feature_gates` asserts the Embedded vs Full tier
split (the `#if os(macOS)` compile gate that drops the four subprocess
bundles from the sandbox-safe slice). Both run only against the Swift
runtime.
