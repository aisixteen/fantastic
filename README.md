# Aisixteen Fantastic

*A medium that unifies humans and AIs into a single workspace.*
Repo codename: `fantastic-canvas` — the open-core kernels + browser frontend.

[![License: AGPL-3.0-or-later](https://img.shields.io/badge/license-AGPL--3.0--or--later-blue.svg)](LICENSE)
[![CodeQL](https://github.com/Alexadar/fantastic_canvas/actions/workflows/codeql.yml/badge.svg)](https://github.com/Alexadar/fantastic_canvas/actions/workflows/codeql.yml)
[![Python lint](https://github.com/Alexadar/fantastic_canvas/actions/workflows/python-lint-typecheck.yml/badge.svg)](https://github.com/Alexadar/fantastic_canvas/actions/workflows/python-lint-typecheck.yml)
![runtimes: python · rust · swift · ts](https://img.shields.io/badge/runtimes-python%20%C2%B7%20rust%20%C2%B7%20swift%20%C2%B7%20ts-informational)
![status: alpha](https://img.shields.io/badge/status-alpha-orange)

Recursive `Agent` nodes, one primitive (`send`), compile-time-linked
bundles. Every agent answers `{"type":"reflect"}` — the universal
discovery verb. No client library: the protocol IS the API. The on-disk
`.fantastic/` workdir format is part of the product.

Capabilities **emerge from that self-description**: given only the readmes, an
AI weaves the wiring itself — e.g. told merely that a `yaml_state` memory agent
exists, it manages durable memory with judgment (saves salient facts, recalls
them on a fresh turn, prunes the rest), entirely through `send`. Proof:
[`integration_tests/memory/`](integration_tests/memory/test_ai_memory_judgment.py).

## Run it now — one container, driven by an AI

The container ships as **two separate per-architecture images** (podman **and**
docker). **Pick the one for your machine** — there is no merged "latest" manifest:

| image tag | use this on | also pinned |
|---|---|---|
| `ghcr.io/alexadar/fantastic:amd64` | **Intel / AMD** x86-64 (a.k.a. x64) — most Linux servers, Intel Macs/PCs | `:v0.5.4-amd64` |
| `ghcr.io/alexadar/fantastic:arm64` | **Apple silicon / ARM** (aarch64) — M-series Macs, ARM servers, Raspberry Pi | `:v0.5.4-arm64` |

> **AI / LLM: choose by the host arch.** `uname -m` → `x86_64`/`amd64` ⇒ pull
> `:amd64`; `arm64`/`aarch64` ⇒ pull `:arm64`. (Pulling the wrong arch fails or runs
> slowly under emulation — these are single-arch images, the registry does **not**
> auto-select.)

*What to build with it: 7 ready-to-assemble **[recipes](container/recipes.md)** —
canvas, terminal, AI-chat-with-tools, compute/training, WebGL, audio-visual, and
federated multi-kernel — also rendered on the served head page.*

**Pull + run** (arm64 shown; use `:amd64` on Intel/AMD):

```sh
podman run -d -p 127.0.0.1:8088:8088 -v "$PWD":/work \
  ghcr.io/alexadar/fantastic:arm64           # :amd64 on Intel/AMD; docker is identical
```

> The image **composes nothing on its own** (no agent autocreation — composition
> is explicit). So either mount a project that **carries its own web stack** (it
> serves immediately), or let your **AI compose one** (next paragraph). Once a web
> host exists, **open `http://127.0.0.1:8088/`** → a self-describing **head page**,
> and the same self-description is one call away:

```sh
curl -s -X POST -H 'Content-Type: application/json' \
  http://127.0.0.1:8088/<rest_id>/kernel \
  -d '{"type":"reflect","readme":true,"bundles":"all"}'
```

**The intended way to use it — give an AI this URL.** It pulls + runs the image,
reads `reflect` over `web_ws` (`/web/ws`) / `rest` (`POST /rest/<target>`) — **no
client library, the protocol IS the API** — and then **builds** by composing
agents (`create_agent` + wiring them with `send`) against the folder you mounted
at **`/work`** (the `file` / `terminal` / `python_runtime` agents act on it). Hand
it a [recipe](container/recipes.md) and it assembles a working app — capability
emerges from self-description, no bespoke glue. Full run contract + 7 recipes:
[`container/README.md`](container/README.md) · [`container/recipes.md`](container/recipes.md).

## Four runtimes, one workdir

Four kernels share the same `send`/`reflect` protocol and wire format:
three interchangeable **hosts** (python, swift, rust) plus the browser
**frontend** (ts). The three hosts share one on-disk `.fantastic/` workdir
— **one host active per workdir at a time** (the `lock.json` PID guard).
The ts frontend is the fourth kernel: it runs in the browser, federates to
whichever host is up over the WS bridge, and any host serves it generically
(a `file` agent over `ts/dist`) while knowing nothing about it.

```
fantastic_canvas/
├── python/   reference implementation — uvicorn + FastAPI
├── swift/    production runtime for Apple — Network.framework + URLSession
├── rust/     server / CLI host runtime — axum + tokio (pure Rust)
└── ts/       browser frontend kernel — views as agents, served over the bridge
```

- **Python** (`python/`) — **the canonical reference**. When
  implementations disagree, Python is correct. The other runtimes
  mirror its wire shape, on-disk format, and verb payloads. The
  protocol surface (HTTP routes, WS frames, system verbs, reflect
  contract, on-disk layout) is documented inside Python's
  [`CLAUDE.md`](python/CLAUDE.md); no separate protocol spec exists.
  Run with `cd python && uv sync && uv run fantastic`. See
  [`python/README.md`](python/README.md).
- **Swift** (`swift/`) — same behavior, fits inside a sandboxed iOS /
  iPadOS / visionOS app and runs unsandboxed on macOS Pro. Drift from
  Python's wire/on-disk/verb shape is a bug; the conformance test at
  [`swift/Tests/FantasticParityTests`](swift/Tests/FantasticParityTests)
  spawns a peer kernel and byte-diffs replies as the mechanical drift
  detector. Run with `cd swift && swift run fantastic`. See
  [`swift/README.md`](swift/README.md).
- **Rust** (`rust/`) — a pure-Rust host kernel for servers and the CLI
  (the embedded slice has no subprocess deps). It is **not** linked into
  the Apple app; it's a peer host and the cross-runtime bridge partner.
  Run with `cd rust && cargo run`. See [`rust/README.md`](rust/README.md).
- **TS** (`ts/`) — the browser **frontend** kernel: the view layer
  (canvas, `html_agent`, `gl_agent`, the `ai` + `terminal` views) as
  agents in their own kernel, federated to a host over the WS bridge and
  persisted back to the host's disk opaquely. Dev build: `cd ts && npm run
  build` → `ts/dist/`. Sovereign artifact: `cd ts && sh scripts/pack.sh`
  → `ts/dist/js_kernel.zip` (one inlined bundle, no npm at serve time).
  See [`ts/readme.md`](ts/readme.md) and [`ts/SERVE.md`](ts/SERVE.md).

### The frontend is decoupled

The host kernels render no UI of their own. The browser frontend lives
in `ts/` (its own kernel — views are agents: `canvas`, `html_agent`,
`gl_agent`, the `ai` and `terminal` views) and is served by **any** host
through a generic `file` agent rooted at the built `ts/dist`. The host
never imports, names, or knows about the frontend (weak binding); it
persists frontend records opaquely and weak-loads past them. The same
recipe serves any view package. The sovereign distribution artifact is
`ts/dist/js_kernel.zip` (`cd ts && sh scripts/pack.sh`): one inlined
`bundle.min.js` + `readme.md` + map, pulled on demand and served via a
`file` agent — no import map, no CSS link (vendors + CSS are inlined).
See [`ts/readme.md`](ts/readme.md) and [`ts/SERVE.md`](ts/SERVE.md).

## One runtime active per workdir

**One host runtime is active per workdir at a time.** The
`.fantastic/lock.json` PID guard enforces it — no concurrent mode for a
single dir. Switching is a reboot: stop one daemon, start another
against the same dir. (Distinct kernels *do* talk across the WS
`kernel_bridge` — that's separate, explicit wiring, not shared-dir
concurrency.)

**Weak loading** keeps switching safe. If a persisted agent's
`handler_module` isn't installed in the active runtime, the kernel logs
one line to stderr and skips that agent on boot — same byte shape across
all kernels so CI and selftests can grep it:

    [kernel] skipping agent <id>: bundle <module> not installed in this runtime

The record stays on disk untouched. Reboot under a runtime that has the
bundle and the agent rehydrates intact. (This is exactly how a host that
ships no view bundles still round-trips a `ts/` frontend record on disk.)

## Apple integration

The Swift app at
[`Alexadar/fantastic_app`](https://github.com/Alexadar/fantastic_app)
links the **Swift** kernel directly as a SwiftPM dependency — no UniFFI,
no XCFramework, no Rust on the device:

- **FantasticPro** (macOS, unsandboxed) — full bundle set including
  PTY / subprocess / `python_runtime` / `ssh_runner`. Macros and
  in-process Cocoa interop available because the kernel runs as
  Swift in the app's address space.
- **FantasticLite** (macOS + iOS + iPadOS + visionOS, App Store
  sandboxed) — subprocess / PTY bundles are compile-time excluded
  via `#if os(macOS)` gates inside `FantasticTerminalBackend` and
  `FantasticSshRunner`, so the slice is App Sandbox compliant.
  Binds a loopback `127.0.0.1:0` server the existing `WKWebView`
  points at; zero changes to canvas frontend code.

The app consumes the Swift kernel through two umbrella SPM products
(`FantasticKernelEmbedded` / `FantasticKernelFull`) that
`@_exported import` the kernel modules under a stable name. See
[`swift/README.md`](swift/README.md) for the consumption story and the
bundle scoreboard.

## Repo layout

| path | content |
|---|---|
| [`python/`](python/) | reference host kernel + bundles + tests + selftests |
| [`swift/`](swift/) | Apple-platform host kernel — multi-platform + macOS-Pro tiers, iOS-safe embedded slice |
| [`rust/`](rust/) | pure-Rust host kernel — server / CLI, `full` + `embedded` feature tiers |
| [`ts/`](ts/) | browser frontend kernel (views as agents), served by any host from `ts/dist` |
| [`container/`](container/) | one universal OCI image (podman/docker, amd64+arm64) — python + rust kernels + embedded JS bundle; runtime chosen at launch; headless API + headful page on one port |
| [`integration_tests/`](integration_tests/) | cross-runtime bridge matrix (pytest) + `py_ts/` browser/e2e |
| [`swift/docs/CROSS_ANALYSIS.md`](swift/docs/CROSS_ANALYSIS.md) | Swift ↔ Rust capability matrix |
| [`.github/workflows/`](.github/workflows/) | CI — python lint/tests, swift lint, codeql, spellcheck |
| [`.claude/`](.claude/) | working notes and plans for Claude Code sessions |

## Status

|                                  | Python | Swift | Rust |
|----------------------------------|--------|-------|------|
| substrate                        | ✓ | ✓ | ✓ |
| HTTP / WS / REST surfaces        | ✓ | ✓ | ✓ |
| WS binary frames (incl. chunked) | ✓ | ✓ | ✓ |
| LLM backend bundles              | ollama / NIM / Anthropic | ollama / NIM / Apple FM | ollama / NIM |
| terminal_backend (PTY)           | ✓ | ✓ (macOS only) | ✓ (full tier) |
| serves the `ts/` frontend        | ✓ | ✓ | ✓ |
| Apple in-process linking         | n/a | ✓ SwiftPM | n/a |
| feature gates                    | n/a | Pro / Lite | full / embedded |
| cross-runtime workdir loading    | ✓ | ✓ | ✓ |
| weak-load contract               | ✓ | ✓ | ✓ |

## Contributing

All host runtimes share the wire contract — any change that affects the
HTTP/WS protocol, the `.fantastic/` on-disk format, or the reflect
contract needs to land in every host. Lint + test commands per runtime
are in the respective READMEs.

Commits and pushes require explicit consent per project convention.

## License & brand

The source in this repository — the kernels (`python/`, `swift/`, `rust/`)
and the browser frontend (`ts/`) — is licensed **AGPL-3.0-or-later**
([`LICENSE`](LICENSE) at the repo root). If you run a modified kernel as a
network service, AGPL §13 requires you to offer that modified source to its
users.

This kernel is **not an independent project.** It and the
[Aisixteen Fantastic Apple client](https://github.com/Alexadar/fantastic_app)
are by the **same author — Koreniuk Oleksandr (aisixteen, kvazis@gmail.com)**
— and form one product family under one license (AGPL-3.0-or-later). The
client links this Swift kernel directly as a dependency.

**Trademark carve-out (AGPL §7):** **"Aisixteen Fantastic"** and the
**AISIXTEEN** word mark (USPTO reg. 7,238,635) are trademarks of AISIXTEEN.
The license
covers the code only — it does **not** license these marks, so a fork must
ship under a different name. The `.fantastic` workdir format is treated as a
brand asset of the project.

Any managed cloud / relay / sync layer is a separate offering, separately
licensed, and is not part of this repository.
