# Fantastic Kernel — Swift

Native Swift kernel for Apple platforms. macOS, iOS, iPadOS,
visionOS, tvOS, watchOS. Used in-process by the Apple app — no
UniFFI, no XCFramework, no native subprocess required.

**Python (`../python/`) is the canonical reference for the Fantastic
protocol.** This Swift kernel mirrors Python's wire shape (HTTP
routes, WS frames, system verb payloads), on-disk format
(`.fantastic/` layout, `agent.json` indent=2 pretty), and reflect
contract. Drift from Python is a bug. Mechanical drift detection
runs in [`Tests/FantasticParityTests`](Tests/FantasticParityTests/) —
spawns the Python kernel as a subprocess and byte-diffs JSON replies
against this Swift kernel's output. Gated via `PYTHON_KERNEL_BIN`
env var; skipped cleanly when unset.

## What's in the box

- 1 substrate target (`FantasticKernel`) — actor-based agent
  store, system verbs, persistence, lock-file workdir guard
- 1 JSON target (`FantasticJSON`) — `OrderedDictionary`-backed
  variant so on-disk JSON matches Python's `dict` key order
- 1 bootstrap target (`FantasticKernelStartup`) — `startKernel(...)`
  / `startKernelInMemory(...)` entry points
- 2 shared core targets — `FantasticAICore` (shared LLM machinery:
  history, epoch cancellation, verb dispatch) and `FantasticRunnerCore`
  (shared runner lifecycle: reflect/boot/start/stop dispatch)
- 17 bundle targets (13 multi-platform + 4 macOS-Pro) — see
  scoreboard below. The browser frontend is no longer a native
  bundle: it is served generically from `ts/dist` via a `file`
  agent (weak binding — the host never names the frontend)
- 2 umbrella targets (`FantasticKernelEmbedded`,
  `FantasticKernelFull`) — Apple-app entry points
- 1 CLI executable (`fantastic`)
- 170+ tests across substrate, bundles, parity harness, public-API
  shim

## Tiers

| product | platforms | includes |
|---|---|---|
| `FantasticKernelEmbedded` | iOS, iPadOS, visionOS, tvOS, watchOS, sandboxed macOS | All bundles EXCEPT subprocess-using ones |
| `FantasticKernelFull` | macOS Pro (unsandboxed) | All bundles including `terminal_backend`, `local_runner`, `python_runtime`, `ssh_runner` |

Both tiers are first-class products of this `Package.swift`. The
Apple app declares the dependency as:

```swift
// In the app's project.yml / Package.swift:
.package(path: "../../fantastic_canvas/swift")
// then per target:
.product(name: "FantasticKernelEmbedded", package: "FantasticKernel")
// or:
.product(name: "FantasticKernelFull",     package: "FantasticKernel")
```

The tier split is realized by which product the consuming app
target depends on, plus the `#if os(macOS)` guards inside the
subprocess-using bundle code. No separate wrapper packages — both
products are umbrella targets in `Sources/FantasticKernelEmbedded/`
and `Sources/FantasticKernelFull/` that `@_exported import` the
kernel modules.

## Bundle scoreboard

| bundle | target | tier | role |
|---|---|---|---|
| file | `FantasticFile` | both | sandboxed file storage |
| yaml_state | `FantasticYamlState` | both | durable YAML key-value memory agent (`state.yaml`, disk-is-truth; `mem`/`data` modes) |
| proxy_agent | `FantasticProxyAgent` | both | host-implemented agents (LanguageModel, etc.) |
| tools | `FantasticTools` | both | LLM tool registry |
| scheduler | `FantasticScheduler` | both | cron / interval triggers |
| cli_bundle | `FantasticCliBundle` | both | scripted-CLI surface |
| kernel_bridge | `FantasticKernelBridge` | both | cross-kernel comms. Transports: in-memory + WS (asymmetric; WS targets remote `web_ws`) + `cloud_bridge` (peer↔peer TLS 1.3 mTLS through the zero-trust relay, pinned by Ed25519 pubkey) |
| web | `FantasticWeb` | both | HTTP + WS server (Network.framework) |
| web_ws | `FantasticWebWS` | both | composable WS verb surface (child of a `web` host; `web_ws.tools`) |
| web_rest | `FantasticWebRest` | both | composable REST verb surface (child of a `web` host; `web_rest.tools`) |
| ollama_backend | `FantasticOllamaBackend` | both | local LLM, URLSession AsyncBytes SSE (thin over `FantasticAICore`) |
| nvidia_nim_backend | `FantasticNvidiaNimBackend` | both | hosted LLM, SSE + bearer auth + 429 retry (thin over `FantasticAICore`) |
| foundation_models_backend | `FantasticFoundationModelsBackend` | both | Apple on-device Foundation Models LLM backend (`foundation_models_backend.tools`; gated by `canImport(FoundationModels)`; thin over `FantasticAICore`) |
| apple_kvs | `FantasticAppleKVS` | both | synced cross-device KV store (`apple_kvs.tools`; iCloud `NSUbiquitousKeyValueStore`; LIVE-ONLY — gated on iCloud sign-in + network, no local fallback; verbs `set`/`read`/`delete`/`list`/`watch`; Apple-only, reports unavailable elsewhere) |
| local_runner | `FantasticLocalRunner` | Pro | macOS-only — Process subprocess (thin over `FantasticRunnerCore`) |
| python_runtime | `FantasticPythonRuntime` | Pro | macOS-only — embedded Python |
| ssh_runner | `FantasticSshRunner` | Pro | macOS-only — `ssh -L` tunnel (thin over `FantasticRunnerCore`) |
| terminal_backend | `FantasticTerminalBackend` | Pro | macOS-only — `forkpty` + DispatchIO |

## Wire compatibility

Verb names stay snake_case strings in JSON. Payload shapes match the
Python kernel byte-for-byte. The `kernel_bridge` bundle's in-process
transport pairs two kernels (e.g. Swift ↔ Python) for end-to-end
mixed-runtime tests.

`OrderedDictionary` (from swift-collections) backs the `JSON.object`
variant so `agent.json` byte-for-byte parity holds across runtimes.

## Building

```bash
cd swift
swift build
swift test                              # full suite
swift test --filter FantasticKernel     # substrate only
PYTHON_KERNEL_BIN=<path> swift test \
    --filter FantasticParityTests       # cross-runtime parity (optional)
```

Requires Swift 6.0+ (Xcode 16+).

## Layout

```
swift/
  Package.swift                        SwiftPM workspace
  Sources/
    FantasticJSON/                     JSON enum + parser
    FantasticKernel/                   Agent, Kernel actor, Bundle protocol,
                                       BundleRegistry, persistence, lock, system verbs
    FantasticKernelStartup/            startKernel / startKernelInMemory
    FantasticWeb/                      HTTP + WS server (Network.framework)
    FantasticAICore/                   shared LLM machinery (history, cancellation,
                                       verb dispatch); AI backends are thin over this
    FantasticOllamaBackend/            local LLM SSE
    FantasticNvidiaNimBackend/         hosted LLM SSE
    FantasticFoundationModelsBackend/  Apple on-device LLM
    FantasticAppleKVS/                 synced cross-device KV (iCloud KVS, live-only)
    FantasticRunnerCore/               shared runner lifecycle (reflect/boot/start/stop);
                                       runner bundles are thin over this
    Fantastic{File,ProxyAgent,Tools,Scheduler,
              CliBundle,KernelBridge}/
                                       supporting bundles
    Fantastic{Terminal,Local,Python,Ssh}{Backend,Runner,Runtime}/
                                       macOS-only Pro-tier bundles
    Fantastic/                         `fantastic` CLI executable
    FantasticKernelEmbedded/           Apple-app umbrella target (Lite tier)
    FantasticKernelFull/                Apple-app umbrella target (Pro tier, macOS-only consumer)
  Tests/
    Fantastic*Tests/                   per-target unit suites
    FantasticParityTests/              cross-runtime byte-diff harness
  docs/
    CROSS_ANALYSIS.md                  capability matrix vs the historical Rust port
    MIGRATION.md                       how the Apple app dropped UniFFI for native Swift
```

## Public API contract

The Swift kernel exposes the same method names + payload shapes the
Apple app already consumes (preserved from the historical UniFFI
surface to keep the app's import lines unchanged through the
migration). Coordinated communication with `app-claude` before any
breaking change to:
- `Kernel.sendJson(targetId:payloadJson:)`
- `Kernel.sendJsonAs(senderId:targetId:payloadJson:)`
- `Kernel.proxyEmit(agentId:eventJson:)`
- `Kernel.registerProxyAgent(agentId:host:)`
- `Kernel.registerTool` / `unregisterToolsBySender` / `listToolsForLlm`
- `ProxyAgent` protocol (`handle`, `onBoot`, `onDelete`)
- HTTP routes (`/<agent_id>/`, `/<agent_id>/ws`, `/_assets/*`,
  `/_fantastic/transport.js`)
- Bundle names: `proxy_agent.tools`, `tools.tools`, `file.tools`,
  `web.tools`, etc.

## Third-party dependencies

- [swift-collections](https://github.com/apple/swift-collections) 1.1+
  — `OrderedDictionary` for `JSON.object` key-order preservation
- [Yams](https://github.com/jpsim/Yams) 5.1+
  — YAML serialization for the `yaml_state` durable-memory agent

That's it — no Hummingbird, no Vapor, no NIO directly. HTTP + WS run
on `Network.framework`; LLM backends run on `URLSession.AsyncBytes`.

## License

AGPL-3.0-or-later, same as the rest of the project. See `../LICENSE`.

---

*Part of **Aisixteen Fantastic** — licensed **AGPL-3.0-or-later** ([`../LICENSE`](../LICENSE)). "Aisixteen Fantastic" and "AISIXTEEN" (USPTO reg. 7,238,635) are trademarks of AISixteen; the license covers the code only, not the marks — forks must rename. See the [root README](../README.md#license--brand).*
