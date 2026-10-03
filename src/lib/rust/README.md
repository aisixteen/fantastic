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

## The on-device backend (macOS build)

`fantastic-foundation-models-backend` (`foundation_models_backend.tools`) is the
rust half of ONE implementation of Apple's Foundation Models calls: the swift
`FantasticFMCore` (`../swift/Sources/FantasticFMCore`) is linked as the static
library `libFantasticFMBridge.a` through its C ABI
(`../swift/Sources/FantasticFMBridge/fm_bridge.h`), so the swift and rust
bundles run the same code and answer and refuse in the same words. Python has
no such backend (Apple-only native).

- **macOS target:** the crate's `build.rs` runs `swift build -c release
  --product FantasticFMBridge --arch <arm64|x86_64>` over `../swift` into
  `target/…/out/swift-build` (the swift package's own `.build` is untouched), and
  links the archive, the Foundation and FoundationModels frameworks and the
  Swift runtime (paths from `xcrun`). It needs Xcode with a macOS 26+ SDK. A
  failing swift build fails the cargo build — no fallback. FoundationModels is
  linked WEAK, so the binary still loads on a macOS without it (and the bundle
  refuses there, naming the platform).
- **Deployment target 14.0:** the swift objects are built for macOS 14, so
  `.cargo/config.toml` sets `MACOSX_DEPLOYMENT_TARGET=14.0` (rustc defaults to
  11.0, where the Swift runtime links through `@rpath` and the binary aborts in
  dyld). `build.rs` refuses a target below 14.0, naming the variable — another
  workspace linking this crate must set it too.
- **Every other target:** nothing is built or linked; the agent's `send` fails
  `the on-device model is unavailable: platform not macOS 26+ — this is <X>`.
- **Both tiers:** registered in `full` and `embedded` (no subprocess and no
  dlopen at run time — the sandboxed macOS tier may run it), as swift's registry
  does on every platform.
- **Tests:** the default suite uses a fake engine plus the real bridge's
  pre-model refusals (no prompt reaches the model); `FM_LIVE=1 cargo test -p
  fantastic-foundation-models-backend live_on_device_model -- --nocapture` runs
  tiny real prompts. Spec: [`selftest/foundation_models_backend.md`](selftest/foundation_models_backend.md).
- **The window is the model's** (4096 tokens, read from the SDK): ai-core's
  `BackendConfig::model_window` makes it the `context_window` default. The
  kernel's default system block does not fit it (~5,800 tokens); a `send`
  carries a short `system_prompt`.

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
slice excludes anything that touches them at compile time. 16 of 17
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
| `fantastic-credential-store`   | SECRETS BY REF — a ref holds named fields bound to ONE host; `set`/`get` only with a `secret` mark, per-ref readers/writers, "no such ref" to strangers; a ref is DECLARED only by `update_agent` on the store's `refs` — `set` stores a declared ref's value, takes no `host` and never creates one; `forget` drops the value and keeps the declaration; the file is read through ONE `O_NOFOLLOW` open judged on its own descriptor; backends `file` (0600 YAML at an ABSOLUTE path — a relative `file` is refused; python's bytes), `keychain` (the OS keychain, items under `keychain_namespace`), `env` (read-only). Every AI backend reads its key here before each request (`credentials: {store, ref}`); stamps `holds_secrets: true` on itself at boot, so ai-core refuses every model-parsed call that names it; the kernel judges the field first on every agent — exactly `true` or `null` (the operator's clear) at the write, and only the operator lowers a holder's mark (`side: "secret-holder"`) |
| `fantastic-static-loader`      | declarative YAML roster loader — hydrates the declared agents as operator at boot |
| `fantastic-file`               | filesystem-as-agent (the fs edge of the io family; sealed by default, deny-all `allow` list) + the text verbs — read/write/edit/append/undo, version/if_match |
| `fantastic-web`                | axum HTTP host — rendering routes; mounts child call surfaces |
| `fantastic-web-ws`             | WS verb channel (mounted onto a parent web); sealed by default; a `call` frame's `secret` marks ride onto the dispatched item |
| `fantastic-web-rest`           | REST verb channel (mounted onto a parent web); sealed by default; the gate runs BEFORE the body is read, and a body past 16 MiB (io_bridge `MAX_FRAME`) is a 413; `X-Fantastic-Secret` (a JSON array) carries the item's `secret` marks |
| `fantastic-scheduler`          | recurring tasks on a tokio tick; a schedule fires AS ITS CREATOR (`owner`), and a caller sees and drives only its own schedules |
| `fantastic-bridge`             | cross-kernel bridge — WS-only, asymmetric (memory / WS); `forward` re-roots its call's `secret` marks onto the frame, an inbound frame's marks ride onto the dispatched item; a `relay_connector` leg is SEALED for marks (relayd records what it routes unmasked) — a marked `forward` and a marked inbound `call` are refused; a leg's rules are read off its record per frame; frame ids are `<leg>-<boot nonce>-<n>`; a relay leg reads its password from a credential_store ref (`credentials: {store, ref}`) before every dial |
| `fantastic-ollama-backend`     | local LLM via ollama |
| `fantastic-responses-backend`  | the OpenAI Responses API |
| `fantastic-vllm-backend`       | a vLLM server, directly (OpenAI chat wire) — KV-cache session table per endpoint |
| `fantastic-foundation-models-backend` | Apple's on-device Foundation Models system model — links the swift `FantasticFMCore` through its C ABI (`FantasticFMBridge`) on macOS; refuses naming the platform everywhere else (see below) |
| `fantastic-cli-bundle`         | the `cli` render sink (token/done/say/error/status/context to stdout) |

Full-tier-only bundles (subprocess; excluded from the embedded slice):

| crate                          | role |
|--------------------------------|------|
| `fantastic-exec-bridge`        | the process edge — a general-purpose command runner, sealed, gated by a regex allowlist; a terminal is a PTY job (flow control, UTF-8 incremental decode, image-paste); a job is driven (`write`/`resize`/`paste_image`/`stop`/`interrupt`/`clear`) only by the caller that started it, or the operator |

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
│       ├── fantastic-credential-store/    secrets by ref (file / keychain / env)
│       ├── fantastic-static-loader/       declarative YAML roster loader
│       ├── fantastic-file/                fs-as-agent + text verbs
│       ├── fantastic-web/                 axum host + WS/REST router
│       ├── fantastic-web-ws/              WS verb channel
│       ├── fantastic-web-rest/            REST verb channel
│       ├── fantastic-scheduler/           recurring tasks
│       ├── fantastic-bridge/              cross-kernel bridge
│       ├── fantastic-ai-core/             shared LLM machinery (Provider seam; internal)
│       ├── fantastic-ollama-backend/      local LLM (thin binding over ai-core)
│       ├── fantastic-responses-backend/   OpenAI Responses API (thin binding over ai-core)
│       ├── fantastic-vllm-backend/        vLLM, directly (thin binding over ai-core)
│       ├── fantastic-foundation-models-backend/  Apple on-device model (swift FMCore via C ABI)
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
  the header naming the blob's slot in `_binary_path` — a JSON array of steps
  from the frame root, a key as a string (taken literally:
  `["payload", "a.b"]` is the key `"a.b"`), a list index as an int; they are
  routed by `type` exactly like text frames. The body reaches the target AT
  THAT SLOT (`fantastic_kernel::Blob` — the payload's steps beside the bytes),
  so each handler reads its own key as python's does (`paste_image` → `data`,
  `write_stream` → `bytes`), and a reply's body leaves at ITS slot the same
  way; REST's octet leg names it with `X-Fantastic-Bytes-Field` — ONE LITERAL
  top-level key (python's `payload[field] = body`: `a.b` is the key `"a.b"`;
  `Blob::key`), both ways. There is no chunked-upload dialect — a large
  transfer is one frame, or a `write_stream` per offset.
- **permessage-deflate** — the ws leg answers an offer as uvicorn's
  websockets server does and `ws_bridge` offers `permessage-deflate;
  client_max_window_bits` as python's client does; the frames run through
  `fantastic_io_bridge::ws_deflate::DeflateStream` under tungstenite.
- **Raw reply bodies** — ANY verb may answer raw bytes beside its JSON
  (`Bundle::handle_binary`, the one dispatch the kernel runs for every call);
  every leg answers binary exactly when the reply carries a body — a WS binary
  frame, a bridge's binary reply frame, a REST octet reply naming the field.
  There is no verb list and nothing to declare.
- **Streams** — byte-heavy transfers ride as raw bodies in a chunked
  PULL (raw bytes, never base64) through the BATCH, NOT events: `read_stream {path, offset?}
  → (header{next_offset, eof, size}, bytes)` (the **SOURCE** — one chunk,
  stateless cursor via `next_offset`); `write_stream {path, offset?,
  truncate?}` + body bytes (the **SINK**); `pump {source, source_path, sink,
  sink_path?}` (the **PUMP** — a server-side SOURCE→SINK copy by id that never
  touches the bytes; both ends run AS THE PUMP'S CALLER — its own grants on both
  ends). `file_bridge` is the reference SOURCE+SINK; the
  `/<id>/file/<path>` route is read_stream-only.
- **REST** `POST /<rest_id>/<target_id>` body=payload → `kernel.send` → JSON
  (octet when the reply carries a raw body at a top-level key).
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
- `foundation_models_backend.md` — the on-device backend: hermetic suite, the
  opt-in live run, the linked binary
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
