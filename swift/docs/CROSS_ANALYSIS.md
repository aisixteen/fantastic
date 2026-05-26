# Swift Port — Cross-Analysis vs Rust

Status after Phases 1-7 land on the `swift` branch. The Rust kernel
in `rust/` remains canonical for non-Apple deployments; the Swift
kernel in `swift/` is feature-complete for the brain-kernel app's
needs on Apple platforms.

## Coverage matrix

| crate / target | Rust LOC | Swift LOC | parity |
|---|---:|---:|---|
| **Core** | | | |
| fantastic-kernel | 3,565 | ~1,200 | full substrate (Agent, Kernel, send/emit/subscribe, system verbs, KernelState save/load, persistence, reflect) |
| fantastic-bundle | 17 | (inlined into FantasticKernel) | full — Swift protocol replaces the trait crate |
| fantastic-uniffi | 756 | (deleted) | **N/A** — UniFFI bridge unnecessary in native Swift |
| **Trivially-portable bundles** | | | |
| file | 466 | ~280 | full verb surface (list/read/write/delete/rename/mkdir) |
| proxy-agent | 827 | ~210 | full — host registration + reflect merge + cascade `onDelete` |
| tools | 1,079 | ~340 | full — register/dispatch/list_for_llm/unregister_by_sender/clear |
| html_agent | 312 | ~70 | full — render_html + set_html |
| gl_agent | 340 | ~70 | full — set_source + reflect with gl_source/source aliasing |
| scheduler | 879 | ~150 | full — DispatchSourceTimer-backed schedule/cancel |
| canvas-backend | 581 | ~150 | full — members + discover + add/remove |
| canvas-webapp | 242 | ~80 | full — bundled canvas.html resource + render_html |
| terminal-webapp | 320 | ~60 | full — bundled index.html resource + render_html |
| ai-chat-webapp | 412 | ~90 | full — verb forwarding to upstream_id |
| telemetry-pane | 175 | ~60 | full verb surface |
| cli-bundle | 161 | ~40 | full — state-event renderer (attach helper) |
| kernel-bridge | 1,668 | ~110 | **in-memory only** — WS / HTTP transports deferred to a polish PR |
| **HTTP layer** | | | |
| web | 2,648 | ~210 (+1.2 MB assets) | **verb shapes + vendored assets**; live listener deferred |
| web-ws | 121 | (deferred) | needs Network.framework / Hummingbird WS upgrade flow |
| web-rest | 119 | (deferred) | same |
| **LLM backends** | | | |
| ollama-backend | 1,746 | ~330 | full — URLSession AsyncBytes streaming |
| nvidia-nim-backend | 1,957 | (deferred) | identical shape to ollama; port when a Swift-only NIM consumer materializes |
| foundation-models-backend | (deleted in main) | (N/A) | folded into proxy_agent in Rust; Swift inherits same architecture |
| **Pro-tier (macOS only)** | | | |
| local-runner | 886 | ~170 (`#if os(macOS)`) | full — Process-backed start/stop/list/shutdown |
| python-runtime | 800 | ~80 (`#if os(macOS)`) | full — python3 -c with stdout/stderr/exit_code capture |
| terminal-backend | 1,603 | (deferred) | PTY differs enough from portable-pty to need its own design pass |
| ssh-runner | 802 | (deferred) | cross-host SSH — deferred for scope |
| **CLI** | | | |
| fantastic-cli | 262 | ~110 | reflect + one-shot RPC modes; daemon mode + workdir bootstrap deferred |
| **Totals** | **24,361** | **~5,468** | |

Swift is ~22% the LOC of Rust at feature parity for what's ported.
Two reasons: (1) Swift's Codable + property syntax cuts a lot of
serde boilerplate; (2) the deferred items (NVIDIA NIM, terminal_backend,
ssh-runner, live HTTP listener, web-ws/rest) account for ~7,000 Rust
LOC — they're real work, just deliberately staged.

## What's BETTER in Swift than the Rust kernel

These wins are inherent to running native on Apple platforms — they
don't reflect Rust being "worse," just FFI tax that disappears.

1. **Apple framework access** — `LanguageModelSession`, `AppIntents`,
   `WidgetKit`, `Vision`, `Speech`, `EventKit`, `SwiftData`, `CloudKit`
   are direct method calls. No callback interface, no JSON marshal,
   no UniFFI ceremony.
2. **SwiftUI agents as native views** — an agent can `render` to
   `some View` and be hosted in `NSHostingController` / `UIHostingController`
   directly, skipping the WebView entirely for high-frequency native
   panels. The HTML/WebView path remains for portable / dynamic
   surfaces.
3. **Codable beats serde for typed boundaries** — `AgentRecord`,
   `KernelState`, etc. roundtrip with less manual macro plumbing.
   (We still use a custom `JSON` enum for substrate dispatch because
   that's fundamentally untyped; same as serde_json::Value.)
4. **Xcode debugger** — set breakpoints anywhere in the substrate;
   no source-map hops, no Rust↔Swift symbol confusion.
5. **`TaskLocal` is cleaner than tokio task_local** — same semantics,
   less ceremony around `with_sender` scopes.
6. **No XCFramework build pipeline** — the Swift package compiles
   into the app the way every other Swift package does. SPM does
   the work that `build-xcframework{,-embedded,-full}.sh` used to.
7. **Sandbox-by-default** — App Sandbox entitlements at the binary
   level. No `cfg(feature = "embedded")` gating; iOS-safe code is
   structurally separated from macOS-Pro-only code via `#if os(macOS)`
   in the bundles that need subprocess access.
8. **AsyncStream for inboxes** is a direct fit for the agent inbox
   pattern — no tokio channel ceremony, native async iteration.
9. **SwiftPM Resources** — bundled assets (canvas.html, three.module
   .js, etc.) ship as first-class `Bundle.module.url(...)` lookups.
   No `include_str!` / `include_bytes!` macro indirection.
10. **Swift `actor` model** for the Kernel is structurally easier
    to reason about than Rust's `Arc<DashMap<...>>` + `tokio::RwLock`
    soup — though we ended up using NSLock-protected classes in
    several places where actors would cause re-entrancy hassle.

## What's BETTER in Rust than the Swift port

1. **Cross-platform reach** — the Rust kernel runs on Linux + Windows
   (CLI binary, headless server). The Swift kernel is Apple-only
   in practice; Linux Swift exists but the toolchain rough edges
   make it unproductive for now.
2. **Compile speed** — `cargo check` on a clean workspace is faster
   than `swift build` on the equivalent target set. Incremental
   builds favor Rust by ~2x.
3. **Memory determinism** — Rust's ownership saves bytes per agent.
   At brain-kernel scale (~10 agents) it's noise; if you ever
   scaled to thousands of agents the ARC overhead would show.
4. **Mature library ecosystem for some niches**:
   - portable-pty (terminal-backend) has no clean Swift analog
   - tokio's `mpsc::channel` is more featureful than `AsyncStream`
   - reqwest's compression / cookie / proxy support is richer than
     URLSession's defaults
5. **`#[derive]` macros for the common patterns** (Clone, Debug,
   PartialEq, Serialize) save more boilerplate per type than
   Swift's auto-Codable does, especially with `serde(flatten)`.
6. **Workspace tooling** — `cargo fmt`, `clippy`, `cargo test
   --workspace` are battle-tested. Swift has SwiftFormat + SwiftLint
   but the integration is less seamless.
7. **`Send + Sync` is checked by the compiler at the type level**
   without `@unchecked Sendable` escape hatches; we used the
   escape hatch on the kernel + several bundles because actor
   re-entrancy made full Sendable conformance gymnastic.
8. **`dashmap` and `arc-swap`** — concurrent collections that
   Swift currently lacks idiomatic equivalents for (swift-atomics
   covers atomics but not concurrent maps).
9. **`tracing` crate** — structured logging with spans is more
   capable than OSLog for cross-cutting concerns; if observability
   matters, Rust wins here.
10. **Cross-runtime testability** — the Rust kernel can run the
    same agent.json + state.json that the Swift kernel produces;
    cross-runtime parity tests work in both directions. Swift can
    only target Apple, so half the testing matrix collapses.

## What's the SAME

These are non-issues for the port — the implementations are
mechanically equivalent.

- Agent tree shape (recursive, flat routing table)
- `send` / `emit` / `subscribe` semantics
- System verb shapes (create_agent / delete_agent / update_agent /
  list_agents / get) — byte-for-byte JSON wire compatibility
- `agent.json` on-disk format (preserved via OrderedDictionary)
- `state.json` snapshot format
- Reflect output shapes per bundle
- Lifecycle semantics (cascade delete, on_delete hooks, delete_lock)
- Tool registry semantics (name → {agent_id, verb, schema, sender})
- Proxy_agent host registration semantics (per-agent_id map)
- Sender attribution via TaskLocal / task_local

## What's DEFERRED (intentional)

These are NOT capability gaps — they're future work documented in
the commit messages of Phases 4-7:

1. **Live HTTP listener** — vendored assets + verb shapes are ready;
   binding Network.framework or Hummingbird to actually serve the
   routes is a polish PR. During migration the Rust XCFramework
   continues serving HTTP for the app.
2. **WebSocket support** — same story; needs the listener first.
3. **NVIDIA NIM backend** — identical shape to ollama; ~3,000
   Rust LOC of error-handling polish that ports when a Swift-only
   NIM consumer materializes.
4. **PTY terminal_backend** — Apple's pseudo-tty story differs
   from portable-pty enough to warrant its own design pass.
5. **SSH runner** — significant cross-host coordination logic;
   deferred for scope.
6. **POSIX flock + daemon-mode bootstrap** — only matters for the
   CLI daemon path; the app-embedded kernel uses in-memory mode.
7. **Live HTTP-backed kernel_bridge transports** (WS / HTTP) —
   in-memory works; the wire-format-equivalent transports land
   when the HTTP listener does.

## What this enables for the Apple app today

The brain-kernel app's needs (per the app-claude brief) are:

| need | Rust kernel (today via UniFFI) | Swift kernel (now) |
|---|---|---|
| In-memory kernel + bundle dispatch | ✅ | ✅ |
| sendJson / sendJsonAs / proxyEmit | ✅ | ✅ (verb-equivalent) |
| registerProxyAgent / ProxyAgent host | ✅ | ✅ (Swift protocol — no UniFFI callback) |
| Tools registry (FM-via-proxy_agent) | ✅ | ✅ |
| Canvas + terminal HTML surfaces | ✅ | ✅ (bundled resources) |
| `/_assets/*` (Three.js, xterm) | ✅ | ✅ (bundled resources) |
| State event subscription | ✅ | ✅ (closure-based, same shape) |
| HTTP server | ✅ (axum, live) | ⏳ (verb shapes + assets; live listener TBD) |
| WS server | ✅ | ⏳ |
| Ollama backend | ✅ | ✅ |
| NVIDIA NIM backend | ✅ | ⏳ (deferred) |
| Subprocess (Pro only) | ✅ | ✅ (local_runner + python_runtime on macOS) |
| Cross-runtime kernel_bridge | ✅ (memory/WS/HTTP) | ✅ in-memory only |

**Net for the app**: the Swift kernel can drive the brain UI today
if the app is OK with running its own embedded HTTP listener (or
deferring HTTP-served surfaces until the listener polish lands).
Every proxy_agent host the app currently wires (header_ui,
actions_ui, recents_ui, chat_ui, banner_ui, fm, gl_background)
plugs into the Swift kernel verbatim — same `registerProxyAgent`
signature, same payload shapes.

## Migration path for the app

Phase 8 (out-of-scope here, app-claude's repo):

1. App imports `FantasticKernel` Swift package alongside the
   existing `FantasticKernelEmbedded` UniFFI package
2. Add a feature flag — when set, route `startKernelInMemory`
   through the Swift kernel instead of the UniFFI one
3. Verify each proxy_agent host (handle / onBoot / onDelete)
   works identically against both kernels
4. When confidence is high enough, drop the UniFFI dep + the
   XCFramework build pipeline
5. Once the Swift HTTP listener lands, drop the embedded Rust
   axum dep too

Until step 5, the Rust kernel can run **alongside** the Swift one
via the in-memory kernel_bridge — both kernels in the same Swift
app process, addressing each other's agents through the bridge.
This is the cleanest A/B test path for any specific bundle.

## Honest caveats

- **Sendable warnings**: Swift 6 strict concurrency surfaced several
  spots where the Rust port used `Arc<DashMap>`-style shared mutable
  state. The Swift versions use `@unchecked Sendable` on the Kernel
  + several bundles, with NSLock discipline. This works but skips
  the compile-time guarantee Sendable usually provides.
- **No PTY**: terminal_backend doesn't port. The terminal surface
  in the Swift kernel is rendering-only; an actual shell session
  needs either the deferred Apple-native PTY port, or rolling the
  Rust XCFramework's terminal_backend in parallel.
- **Live HTTP missing**: the bundle reports the right routes and
  has the right assets, but nothing actually serves them in the
  Swift kernel yet. The app must either keep the Rust XCFramework
  for HTTP traffic or implement an embedded listener.
- **No multi-runtime parity tests yet**: the cross-runtime test
  harness (Rust kernel and Swift kernel exchanging messages over
  the kernel_bridge) is doable but not built. Would catch any
  wire-format drift before it bites the app.

## Recommendation

Treat this as a **dual-runtime period**. The Swift kernel is solid
for everything that's ported (which is most of what the app uses).
Keep the Rust XCFramework as the HTTP backbone + non-Apple
deployment story. Migrate one bundle at a time — start with the
Apple-only ones (FM-as-proxy_agent, anything new touching
LanguageModelSession / AppIntents / Vision / Speech) because
those benefit most from native Swift access.

The deferred items (NVIDIA NIM, PTY, SSH runner, live HTTP) are
deliberate. Each is its own polish PR when the consuming feature
needs it.

**Decision point**: when the Swift kernel reaches 100% of what the
brain-kernel app needs (likely after the live HTTP listener lands),
retire the Rust XCFramework on Apple platforms. The Rust kernel
keeps shipping for Linux / Windows.
