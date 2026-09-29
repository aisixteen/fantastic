# fantastic-kernel

A medium that unifies humans and AIs into a single workspace.
Recursive `Agent` nodes, one primitive (`send`), compile-time-linked
bundles. Every agent answers `{"type":"reflect"}` — the universal
discovery verb. No client library: the protocol IS the API.

## Status

Production runtime — full parity with the Python reference kernel.

|                                              | value                                   |
|----------------------------------------------|-----------------------------------------|
| Bundles                                      | **16** (15 iOS-safe + 1 full-tier subprocess) |
| Cargo tests passing                          | **1091** (workspace, default features)  |
| `./scripts/quality.sh`                       | 8 / 8 PASS (compile, fmt, clippy, test, deny, audit, machete, tree) |
| Feature gates                                | `full` (default) / `embedded` (no-subprocess) |
| Embedded slice (`cli --no-default-features --features embedded`) | clean compile, subprocess-using bundles excluded |
| Cross-runtime workdir                        | byte-identical `.fantastic/` round-trip |
| Cold start                                   | 30 / 30 / 88 ms (virgin / hydrate / boot-to-listening) |
| Prebuilt binaries                            | 4 targets (macOS arm64+x86_64, Linux x86_64+aarch64) via [RELEASING.md](RELEASING.md) |

## Why Rust

A single portable native binary — same workdir format and HTTP / WS
contract everywhere it runs (server, Mac desktop, Linux). Pure Rust:
no foreign-language bindings.

## Architecture

```
┌──────────────────────────────────────────────────────────────────────────┐
│  SUBSTRATE  (crates/fantastic-kernel/)                                   │
│   Agent  — recursive node; .send / .emit / .create / .delete             │
│   Kernel — tree-wide ctx (flat agents index, state subs, bundle reg)     │
│   System verbs (create/delete/update/list_agents) baked into Agent.      │
└────────────────────────┬─────────────────────────────────────────────────┘
                         │  agent ⇌ agent ⇌ agent (agent.send)
       ┌─────────────────┼─────────────────────────────┐
       ▼                 ▼                             ▼
   ┌────────┐      ┌────────────┐              ┌────────────────┐
   │ kernel │      │ web        │              │ scheduler /    │
   │ cli    │      │ (axum)     │              │ runners /      │
   │ file   │      │ HTTP+WS+   │              │ backends /     │
   │ ...    │      │ REST       │              │ ...            │
   └────────┘      └─────┬──────┘              └────────────────┘
                         │
                         ▼ HTTP + WS frames (text + binary)
   ┌─────────────────────────────────────────────────────────────────────┐
   │   BROWSER: the ts kernel LIBRARY (src/lib/ts), booted by an app    │
   │  (the CLI's canvas). No view bundles here — the host serves the     │
   │  app's page through a file agent and never names the frontend.    │
   └─────────────────────────────────────────────────────────────────────┘
```

Host kernels render no UI and ship no view bundles. The frontend is the
weak-bound ts kernel LIBRARY (`src/lib/ts`), booted in the browser by an app
(the CLI's canvas, `src/tui/web/canvas`) whose page is served generically via a
`file` agent — the host never names it.

THE KERNEL KEEPS NO CONVERSATION. Every message is a row in a MEMORY STORE agent
(`fantastic-ram-store`: in RAM, gone with the process; a durable store answers
the same verbs). The root names it in `memory_store_id` and `kernel_start`
refuses without one; the kernel flushes its execution rows (call/settle/ack/
flush) there, and `ledger`, dead letters (`on_deadletter`, `ack_deadletter`) and
named-call dedup read them back. Each AI agent names its own store; context moves
between agents only by grant (`share` → `fork_from`). Lifecycle verbs:
`kernel_start`, `shutdown_agents`, `kernel_stop`. Manual: `reflect kernel
readme=memory`. A static `readme-contract` lint
(`integration_tests/decoupling/test_readme_contract.py`) scans every host
bundle's readme/sentence across python/rust/swift and fails on any
client-intent word (iframe, transport.js, browser, …), so the host stays
client-agnostic by construction.

## Run

```bash
cd src/lib/rust
cargo build --release --bin fantastic_kernel
BIN=$(pwd)/target/release/fantastic_kernel

# One-shot RPCs:
$BIN reflect                                  # substrate identity + tree of root (id="kernel_state")
$BIN reflect kernel_state bundles=all                 # add the registered-bundle catalog
$BIN reflect kernel_state readme=true                 # attach the agent's readme (transport/wire docs)
$BIN kernel_state list_agents                         # every agent in this workdir
$BIN kernel_state create_agent handler_module=web.tools id=w port=8888

# Daemon mode (blocks if a `web` agent is persisted in the workdir):
$BIN
# → "fantastic: daemon up. N agent(s) loaded. Ctrl-C to stop."
```

`reflect` is the param-driven discovery verb: with no flags it returns
the addressed agent's substrate identity + nested `tree`; `bundles=all|ids`
adds the registered-bundle catalog and `readme=true` attaches the agent's
readme. There is no separate "primer" — transport and wire-protocol docs
live in the root readme (reachable via `reflect readme=true`).

Composition rule: `fantastic` blocks only when the workdir has a
`web` agent persisted (HTTP daemon) or `stdin` is a tty (REPL).
Otherwise it exits silently.

## Feature gates

Two compile-time tiers gate which bundles ship in the binary:

```toml
default = ["full"]
full     # CLI + server + macOS Pro + Linux unsandboxed
embedded # iOS Lite, iPadOS, visionOS, sandboxed macOS
```

**`full`** includes every ported bundle. The subprocess-spawning bundle
(`exec_bridge` — commands and PTY jobs, the one spawner) is gated to this feature.

**`embedded`** compiles without any subprocess code. iOS app sandboxes
forbid `fork()` / `Process` / dynamic library loading; the embedded
slice excludes anything that touches them at compile time. 15 of 16
bundles ship under embedded — the iOS-safe ceiling.

Switch with `--no-default-features --features embedded`:

```bash
cargo check  -p fantastic-cli    --no-default-features --features embedded
```

Passes clean — that's the contract the sandboxed/no-subprocess tier
ships against.

## Bundle map

The frontend ships no native view bundles — an app's page is served
generically via a `file` agent (the host never names the frontend).

Bundles that compile under either tier:

| crate                          | role |
|--------------------------------|------|
| `fantastic-core`               | the `kernel_state.tools` root loader (id="kernel_state") — records, blobs, the record-owner guard |
| `fantastic-ram-store`          | the MEMORY STORE in RAM — every message (AI conversations, the kernel's execution rows), fold refs, grants, search, inject/dump |
| `fantastic-static-loader`      | declarative YAML roster loader — hydrates the declared agents as operator at boot |
| `fantastic-file`               | filesystem-as-agent (the fs edge of the io family; sealed by default, deny-all `allow` list) + the text verbs — read/write/edit/append/undo, version/if_match |
| `fantastic-web`                | axum HTTP host — rendering routes; mounts child call surfaces |
| `fantastic-web-ws`             | WS verb channel (mounted onto a parent web); sealed by default |
| `fantastic-web-rest`           | REST verb channel (mounted onto a parent web); sealed by default |
| `fantastic-scheduler`          | recurring tasks on a tokio tick |
| `fantastic-bridge`             | cross-kernel bridge — WS-only, asymmetric (memory / WS) |
| `fantastic-ollama-backend`     | local LLM via ollama |
| `fantastic-nvidia-nim-backend` | NVIDIA NIM LLM (OpenAI-compatible) |
| `fantastic-llamacpp-backend`   | local LLM via llama-server (OpenAI-compatible) |
| `fantastic-anthropic-backend`  | Claude via the Anthropic Messages API |
| `fantastic-responses-backend`  | the OpenAI Responses API |
| `fantastic-cli-bundle`         | the `cli` render sink (token/done/say/error/status/context to stdout) |

Full-tier-only bundles (subprocess; excluded from the embedded slice):

| crate                          | role |
|--------------------------------|------|
| `fantastic-exec-bridge`        | the process edge — a general-purpose command runner, sealed, gated by a regex allowlist; a terminal is a PTY job (flow control, UTF-8 incremental decode, image-paste) |

Internal / test crates:

| crate                          | role |
|--------------------------------|------|
| `fantastic-ai-core`            | shared LLM-agent machinery behind a `Provider` seam — memory-store conversation, `<call>` tag grammar, compaction (folds), grants; the AI backends bind to it |
| `fantastic-scenario-probes`    | TEST-ONLY fixture agents the shared e2e scenarios name (behind a feature) |

## Workspace layout

```
rust/
├── Cargo.toml                         workspace root
├── crates/
│   ├── fantastic-kernel/              substrate (Agent + Kernel + send/emit/watch/reflect)
│   ├── fantastic-bundle/              bundle trait every bundle re-exports
│   ├── fantastic-cli/                 the `fantastic_kernel` headless host binary
│   └── bundles/
│       ├── fantastic-core/                kernel_state.tools root loader
│       ├── fantastic-ram-store/           the memory store (in RAM)
│       ├── fantastic-static-loader/       declarative YAML roster loader
│       ├── fantastic-file/                fs-as-agent + text verbs
│       ├── fantastic-web/                 axum host + WS/REST router
│       ├── fantastic-web-ws/              WS verb channel
│       ├── fantastic-web-rest/            REST verb channel
│       ├── fantastic-scheduler/           recurring tasks
│       ├── fantastic-bridge/              cross-kernel bridge
│       ├── fantastic-ai-core/             shared LLM machinery (Provider seam; internal)
│       ├── fantastic-ollama-backend/      local LLM (thin binding over ai-core)
│       ├── fantastic-nvidia-nim-backend/  NVIDIA NIM LLM (thin binding over ai-core)
│       ├── fantastic-llamacpp-backend/    llama-server LLM (thin binding over ai-core)
│       ├── fantastic-anthropic-backend/   Claude (thin binding over ai-core)
│       ├── fantastic-responses-backend/   OpenAI Responses API (thin binding over ai-core)
│       ├── fantastic-cli-bundle/          the cli render sink
│       ├── fantastic-exec-bridge/         command runner + PTY jobs (full-tier only)
│       └── fantastic-scenario-probes/     e2e fixture agents (test-only feature)
├── scripts/
│   ├── build-cli.sh                       cargo build --release --bin fantastic_kernel
│   ├── bench-coldstart.sh                 3-metric boot benchmark
│   ├── quality.sh                         canonical pre-push gate
│   └── compat-python.sh                   black-box wire-protocol probes
├── selftest.md                            index + Rust-overlay specs
└── selftest/                              Rust-specific selftest overlays
```

## Bundle model

Bundles register at **compile time** — the CLI crate links the
default set; the `embedded` feature links the no-subprocess subset.
Adding a bundle to a build means adding its crate to the workspace
and calling `reg.register(...)` in the relevant
`register_default_bundles()` site.

## Wire surface

Browsers, embedding apps, and LLM clients consume the kernel through
HTTP + WebSocket:

- **HTTP** `/`, `/_assets/favicon.png`, `/favicon.png`, `/<id>/file/<path>`.
- **WS `/<id>/ws`** — text frames: `call` / `emit` / `watch` /
  `unwatch` / `reply` / `error` / `event`. **Binary frames** carry
  byte-heavy payloads as `[4-byte BE u32 hdr_len][JSON header][raw blob]`,
  the header naming the blob's slot in `_binary_path`; they are routed by
  `type` exactly like text frames. There is no chunked-upload dialect — a
  large transfer is one frame, or a `write_stream` per offset.
- **Streams** — byte-heavy transfers ride the binary channel as a chunked
  PULL (raw bytes, never base64) through the BATCH, NOT events: `read_stream {path, offset?}
  → (header{next_offset, eof, size}, bytes)` (the **SOURCE** — one chunk,
  stateless cursor via `next_offset`); `write_stream {path, offset?,
  truncate?}` + body bytes (the **SINK**); `pump {source, source_path, sink,
  sink_path?}` (the **PUMP** — a server-side SOURCE→SINK copy by id that never
  touches the bytes). `file_bridge` is the reference SOURCE+SINK; the
  `/<id>/file/<path>` route is read_stream-only.
- **REST** `POST /<rest_id>/<target_id>` body=payload → `kernel.send` → JSON.
  `GET /<rest_id>/_reflect[/<target_id>]` for static discovery.
- **`.fantastic/`** — disk-mode workdir state. Python-compatible
  per-agent layout: `agent.yaml` for the root + `agents/<id>/agent.yaml`
  recursively for every child. `lock.yaml` holds the daemon's PID.
  Bundle-local sidecar files (chat history, scheduler state, etc.)
  live next to each agent's `agent.yaml` in its dir.

## State medium — save/load foundation

The kernel's whole agent tree is materializable as a serializable
[`KernelState`] in RAM — never as a file on disk. Two storage modes
pick the *medium*:

| mode | what it does | when |
|---|---|---|
| `StorageMode::Disk(workdir)` | Adapter mirrors each agent record onto `<workdir>/.fantastic/agents/<id>/agent.yaml`. **Dirty binding**: persistence merges kernel-managed fields into the existing JSON — never wholesale-overwrites, never touches sidecar files. Bundles reconcile their own slices when they next touch them. | The standalone CLI; the workspace-kernel embed (Pro Mac, anyone holding a folder). |
| `StorageMode::InMemory` | No filesystem I/O ever. State lives only in `kernel.agents`. The consumer extracts a snapshot via `kernel.save() -> KernelState` and restores via `kernel.load(state)`. | An embedding app's in-RAM "brain" kernel — always running, never touches disk, snapshot persists externally. |

Both modes share the same save/load API. The only difference is
the medium — disk mode also mirrors each agent record to its
`agent.yaml` on every mutation:

```rust
let snapshot: KernelState = kernel.save();          // both modes (pure read)
let json: String          = kernel.save_json();     // both modes (JSON snapshot)
kernel.load(snapshot)?;                              // both modes (replace tree)
kernel.load_json(&json)?;                            // both modes (parse + replace)
```

`save_json()` output is byte-deterministic for equal in-memory state
(agents sorted by id), so it composes with content-addressed storage
and `diff`-style review.

A black-box `scripts/compat-python.sh` runs wire-protocol probes
against the running binary; CI fails on any divergence.

## Cross-runtime workdir compatibility

Same `.fantastic/` directory loads under either Python or Rust
kernel. Records hydrate from identical per-agent `agent.yaml` JSON.
Bundles missing in one runtime log a single skip line and the boot
continues:

    [kernel] skipping agent <id>: bundle <module> not installed in this runtime

Wire-identical across runtimes — AI agents grep this line so the
exact string is contract.

An `exec_bridge` record names its own interpreters in the
`interpreters` allowlist — there is no default and no PATH probing, so
the record itself is the durable, deterministic answer both runtimes
hit on subsequent opens.

See [`selftest/cross_runtime_workdir.md`](selftest/cross_runtime_workdir.md)
for the round-trip test plan.

## Cold start

Release binary's boot budget, measured by `scripts/bench-coldstart.sh`:

| metric                    | target  | latest |
|---------------------------|---------|--------|
| virgin-dir reflect        |  50 ms  |  30 ms |
| 18-agent hydrate reflect  | 100 ms  |  30 ms |
| boot-to-listening (HTTP)  | 200 ms  |  88 ms |

Captured on macOS arm64 (M-series) in release mode against a fresh
tempdir. CI runs the same script with 2× ceilings via
`FANTASTIC_BENCH_RELAXED=1` to absorb cloud-runner variance.

Run locally:

```bash
cd src/lib/rust
cargo build --release --bin fantastic_kernel
./scripts/bench-coldstart.sh
```

## Selftests

Most user-facing behaviour is identical across Python and Rust
runtimes — Python's per-bundle selftests under
[`../python/bundled_agents/*/selftest.md`](../python/bundled_agents/)
drive the wire surface against either binary by swapping `PATH`.

Rust-specific behaviour lives in [`selftest/`](selftest/):

- `feature_gates.md` — `full` vs `embedded` compile matrix
- `cross_runtime_workdir.md` — round-trip workdir loading

See [`selftest.md`](selftest.md) for the index + driving workflow.

## Pre-push checks

> **Working with Claude on this repo?** Run `./scripts/quality.sh`
> (or its individual sections) before every commit you ask Claude
> to make. Claude SHOULD pick the strictest tools available — clippy
> with `-D warnings`, `cargo fmt --check`, `cargo deny`, strict YAML
> parsing on workflow edits — so CI doesn't surface lint issues
> the local toolchain quietly accepted (Rust toolchain version
> skew has burned us twice). No git hooks installed by design;
> the gate is operator-driven via the script + Claude's own
> pre-commit sweep.

Single command — `./scripts/quality.sh` runs the canonical gate
(8 sections): `compile`, `fmt`, `clippy`, `test`, `deny`, `audit`,
`machete`, `tree`. See the script header for what each does and
`--install` for fetching missing tools (`cargo-deny`, `cargo-audit`,
`cargo-machete`).

```bash
cd src/lib/rust
./scripts/quality.sh                    # default — skip missing tools
./scripts/quality.sh --install          # install missing tools first
./scripts/quality.sh --section deny     # run one section only
```

The longer breakdown still works if you want to drive sections by hand:

```bash
cargo check --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
cargo test --workspace
cargo check -p fantastic-cli --no-default-features --features embedded
./scripts/compat-python.sh
./scripts/bench-coldstart.sh
```

CI runs the workspace tests on Linux + macOS via
`.github/workflows/rust-build.yml`. Release builds (4-target tarballs)
are driven by `.github/workflows/release-rust.yml` — see
[`RELEASING.md`](RELEASING.md) for how to cut a release.

## License & brand

Licensed **AGPL-3.0-or-later** ([`../LICENSE`](../LICENSE)). "Aisixteen
Fantastic" and "AISIXTEEN" (USPTO reg. 7,238,635) are trademarks of AISixteen;
the license covers the code only, not the marks — forks must rename. See the
[root README](../README.md#license--brand).
