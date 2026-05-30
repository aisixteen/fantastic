# fantastic-canvas

A medium that unifies humans and AIs into a single workspace.
Recursive `Agent` nodes, one primitive (`send`), compile-time-linked
bundles. Every agent answers `{"type":"reflect"}` — the universal
discovery verb. No client library: the protocol IS the API.

## Two runtimes, one workdir

The kernel ships in two implementations that share the same on-disk
format (`.fantastic/`) and the same HTTP + WebSocket wire protocol:

```
fantastic_canvas/
├── python/   reference implementation — uvicorn + FastAPI, 510 tests
└── swift/    production runtime for Apple — Network.framework + URLSession, 122 tests
```

- **Python** (`python/`) — **the canonical reference**. When
  implementations disagree, Python is correct. Other runtimes
  mirror its wire shape, on-disk format, and verb payloads. The
  protocol surface (HTTP routes, WS frames, system verbs, reflect
  contract, on-disk layout) is documented inside Python's
  [`CLAUDE.md`](python/CLAUDE.md); no separate protocol spec
  exists. Run with `cd python && uv sync && uv run fantastic`.
  See [`python/README.md`](python/README.md).
- **Swift** (`swift/`) — same behavior, fits inside a sandboxed iOS
  / iPadOS / visionOS app and runs unsandboxed on macOS Pro.
  Mirrors Python; drift from Python's wire/on-disk/verb shape is
  a bug. The cross-runtime conformance test at
  [`swift/Tests/FantasticParityTests`](swift/Tests/FantasticParityTests)
  spawns the Python kernel and byte-diffs replies as the
  mechanical drift detector. Run with `cd swift && swift run
  fantastic`. See [`swift/README.md`](swift/README.md).

**One runtime is active per workdir at a time.** The existing
`.fantastic/lock.json` PID guard enforces it — there is no concurrent
mode, no bridge between them. Switching is a reboot: stop one daemon,
start the other against the same dir.

**Weak loading** keeps switching safe. If a persisted agent's
`handler_module` isn't installed in the active runtime, the kernel
logs one line to stderr and skips that agent on boot — same byte
shape across both kernels so CI and selftests can grep it:

    [kernel] skipping agent <id>: bundle <module> not installed in this runtime

The record stays on disk untouched. Reboot under the runtime that
has the bundle and the agent rehydrates intact. Wipe-and-rebuild
safe.

## Apple integration

The Swift app at
[`Alexadar/fantastic_app`](https://github.com/Alexadar/fantastic_app)
links the Swift kernel directly as a SwiftPM dependency — no UniFFI,
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
[`swift/README.md`](swift/README.md) for the consumption story and
the bundle scoreboard.

## Repo layout

| path | content |
|---|---|
| [`python/`](python/) | reference kernel + 21 bundles + 510 tests + selftests |
| [`swift/`](swift/) | Apple-platform kernel — 20 bundles (16 multi-platform + 4 macOS-Pro), 122 swift tests, iOS-safe embedded slice |
| [`swift/docs/CROSS_ANALYSIS.md`](swift/docs/CROSS_ANALYSIS.md) | capability matrix vs the historical Rust port |
| [`swift/docs/MIGRATION.md`](swift/docs/MIGRATION.md) | how the Apple app dropped UniFFI for the native Swift kernel |
| [`.github/workflows/`](.github/workflows/) | CI — `python-*.yml` (lint, tests), `swift-build.yml` (build), `codeql.yml`, `spellcheck.yml` |
| [`.claude/`](.claude/) | working notes and plans for Claude Code sessions |

A native Rust port of the same kernel lived under `rust/` through
phase 8 of the Swift port (commits `61baeac` → `3a5bf8d`); the Apple
app linked it via UniFFI until the Swift kernel reached parity. Both
the Rust workspace and its UniFFI bindings have been retired from
this repository — `git log -- rust/` recovers the full history.

## Status

|                                  | Python                | Swift                                |
|----------------------------------|-----------------------|--------------------------------------|
| substrate                        | ✓ 510 tests           | ✓ 122 tests                          |
| HTTP / WS / REST surfaces        | ✓                     | ✓ (single port, dynamic mount)       |
| WS binary frames (incl. chunked) | ✓ single-frame        | ✓ single + chunked uploads           |
| canvas in browser                | ✓                     | ✓                                    |
| LLM bundles (ollama / NIM)       | ✓                     | ✓                                    |
| terminal_backend (PTY)           | ✓                     | ✓ (macOS only — `#if os(macOS)`)     |
| Apple in-process linking         | n/a                   | ✓ SwiftPM, no XCFramework            |
| Feature gates (Pro / Lite)       | n/a                   | ✓ subprocess bundles excluded from Lite |
| Cross-runtime workdir loading    | ✓                     | ✓ (round-trip verified)              |
| Weak-load contract               | ✓                     | ✓ matches Python byte-for-byte       |

## Contributing

Both runtimes share the wire contract — any change that affects the
HTTP/WS protocol, the `.fantastic/` on-disk format, or the reflect
primer needs to land in both. Lint + test commands per runtime are
in the respective READMEs.

Commits and pushes require explicit consent per project convention.

## License

Apache-2.0. See [`LICENSE`](LICENSE) at the repo root.
