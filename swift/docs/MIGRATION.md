# Migration Guide — Rust XCFramework → Native Swift Kernel

For the Apple-platform app that consumes `FantasticKernelEmbedded`
and/or `FantasticKernelFull`. After phases 8A–8I land on `main`, the
SPM packages at `rust/packaging/FantasticKernel{Embedded,Full}/`
back onto the native Swift kernel at `swift/` instead of the Rust
XCFramework.

**You should not need to change any `import` lines or call sites.**
The public API (`startKernelInMemory`, `startKernel`, `Kernel`,
`ProxyAgent`, `StateListener`, `registerProxyAgent`, `sendJson`,
`sendJsonAs`, `proxyEmit`, `registerTool` / `unregisterTool` /
`unregisterToolsBySender` / `listToolsForLlm`, `dispatchTool`,
`save`, `load`, `shutdown`, `httpPort`) is identical.

What changes underneath:
- The `FantasticUniFFIEmbedded.xcframework` / `FantasticUniFFIFull
  .xcframework` binary targets are gone from the packages.
- The packages now `.package(path: "../../../swift")` and re-export
  the Swift kernel's modules.
- `cargo build`, `scripts/build-xcframework-*.sh`, and the Rust
  workspace as a whole keep working. Nothing in the Apple app links
  the resulting XCFramework any more — Rust still emits the
  bindings for non-Apple-app consumers and cross-runtime parity
  testing.

## Step-by-step

### 1. Pull the `swift` branch into `main`

The Swift kernel + the repointed SPM shims merge together. Your
existing `project.yml` packages stanza for FantasticKernelEmbedded
+ FantasticKernelFull keeps pointing at the same paths:

```yaml
packages:
  FantasticKernelEmbedded:
    path: ../../fantastic_canvas/rust/packaging/FantasticKernelEmbedded
  FantasticKernelFull:
    path: ../../fantastic_canvas/rust/packaging/FantasticKernelFull
```

No edits needed here.

### 2. Clean derived data + rebuild

The build system needs to re-resolve packages because the
underlying dependency graph changed:

```bash
rm -rf ~/Library/Developer/Xcode/DerivedData/Fantastic-*
xcodegen
```

Then build either target:

```bash
xcodebuild -scheme "Fantastic Lite" -destination 'generic/platform=iOS' build
xcodebuild -scheme "Fantastic Pro" -destination 'generic/platform=macOS' build
```

If either fails on the first build with "package missing," resolve
manually once:

```bash
xcodebuild -resolvePackageDependencies -scheme "Fantastic Lite"
```

### 3. Smoke-test the brain kernel path

The proxy_agent host registration is the most-touched path. After
build, run the app and check each `ProxyAgentHost` (HeaderProxyHost,
ActionsProxyHost, RecentsProxyHost, ChatProxyHost, BannerProxyHost,
FoundationModelsProxyHost, GlBackgroundProxyHost) registers + receives
verbs. The wire shape is identical; only the implementation language
under the hood changed.

If a host's `handle` returns a JSON string differently than before,
look for these two known shifts:

- **OrderedDictionary key order**: Swift kernel preserves insertion
  order on JSON objects (matches Rust's `preserve_order` serde
  feature). If the app was tolerating Rust's order but breaks on
  Swift's, audit the consuming JSON parser — RFC 8259 says key order
  is insignificant, but some flaky tools assume alphabetical.
- **Error envelope shape**: the Swift kernel returns `{error: "..."}`
  on missing agents / handlers; the Rust kernel returned the same
  shape. If anything changed, it's a bug — report.

### 4. Probe `FantasticKernelEmbeddedInfo.isSwiftNative`

The Swift-backed packages expose a flag:

```swift
import FantasticKernelEmbedded

if FantasticKernelEmbeddedInfo.isSwiftNative {
    print("running on native Swift kernel")
}
```

Use this in app analytics or a debug overlay if you want explicit
runtime confirmation.

## Rollback procedure

If something regresses, the rollback is one git revert:

```bash
git revert <swift-kernel-merge-commit>
```

The XCFramework build scripts (`rust/scripts/build-xcframework-*.sh`)
never stopped working. Rebuild the framework + the prior
`Package.swift` (binaryTarget-based) version restores the Rust
backend in a single commit.

For per-feature rollback (Swift kernel for chat, Rust for terminal,
etc.) — the kernel_bridge bundle in either kernel can in-memory-attach
to a Rust kernel running alongside the Swift one. Both backends can
co-exist in a single Apple app process during a staged migration.

## Sub-phase dependencies (what unblocks what)

Each sub-phase of phase 8 unlocks specific app capabilities:

| sub-phase | unlocks |
|---|---|
| 8A — public API shims | Apple app can call the Swift kernel's `sendJson` / `registerProxyAgent` / etc. |
| 8I — SPM package repoint | `import FantasticKernelEmbedded` resolves to Swift kernel. Migration is technically possible at this point. |
| 8H — daemon + flock | CLI daemon mode works; Disk-mode kernels coordinate properly. |
| 8D — NVIDIA NIM | Chat against NIM works through Swift kernel. |
| 8B — HTTP listener | Canvas / terminal WebView surfaces render through Swift kernel's HTTP. |
| 8C — WebSocket | `transport.js`-driven WS clients (browser surfaces) light up. |
| 8E — bridge transports | Remote-kernel-over-WS / HTTP attach. |
| 8F — ssh_runner | Cross-host workspace mount. macOS Pro only. |
| 8G — terminal_backend PTY | Real shell sessions through xterm.js. macOS Pro only. |

You can roll the migration out feature-by-feature by enabling each
sub-phase as it lands; everything in front of it keeps working
through the prior runtime (kernel_bridge in-memory link).

## Known caveats

1. **`@unchecked Sendable` usage**: the Swift kernel uses
   `@unchecked Sendable` on `Kernel`, `Agent`, and several bundles
   to opt out of strict-concurrency compile-time checks. State is
   protected by NSLock at every mutation site. If a future Swift
   release tightens `@unchecked` semantics, several files will
   need audit.
2. **No live HTTP-served `favicon.png`**: 8B serves a 404 for
   `/favicon.ico` and `/favicon.png`. The Rust kernel served a
   bundled 602-KB PNG. Vendoring the favicon is a tiny polish PR
   if a visible-tab-icon issue surfaces.
3. **`terminal_backend` flow control simplified**: no per-stream
   5 MB cap, no ack-per-5K-chars window. Apple PTY backpressure
   covers most cases; revisit if a flood actually shows up.
4. **NIM tool-call behavior**: SSE delta aggregation matches Rust's
   shape but hasn't been spot-checked against a live NIM endpoint
   for the brain-kernel app. If chat with tools regresses, this is
   the path to inspect first.
5. **No cross-runtime parity test in CI yet**: 8J ships a harness
   that boots both kernels and diffs verb replies; until that
   lands, drift detection is manual.

## Versioning

The Swift kernel packages expose `version = "0.2.0-swift"`. The
Rust workspace keeps its own version in `Cargo.toml`. They are
independent from this commit forward; bumps don't need to mirror.

The SPM Package.swift files at the Rust-side path
(`rust/packaging/...`) keep their existing locations so app
project.yml entries don't move. Files renamed under those paths
are reflected in this commit.
