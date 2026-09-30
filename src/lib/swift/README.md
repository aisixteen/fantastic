# Fantastic Kernel — Swift

Native Swift kernel for Apple platforms. macOS, iOS, iPadOS,
visionOS, tvOS, watchOS. Used in-process by the Apple app — no
native subprocess required.

**Python (`../python/`) is the canonical reference for the Fantastic
protocol.** This Swift kernel mirrors Python's wire shape (HTTP
routes, WS frames, system verb payloads), on-disk format
(`.fantastic/` layout, `agent.yaml` byte-identical YAML), and reflect
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
- 1 shared core target — `FantasticAICore` (shared LLM machinery: the
  conversation in the agent's memory store, the `<call>` tag grammar,
  compaction/folds, grants, verb dispatch); the AI backends are thin over it
- the MEMORY STORE (`FantasticRamStore`): the kernel keeps NO conversation —
  every message (AI conversations, the kernel's execution rows) is a row in a
  memory store agent. The root names it in `memory_store_id` and
  `kernel_start` refuses without one; `ledger`, dead letters
  (`on_deadletter`, `ack_deadletter`) and named-call dedup read the store back;
  context moves between agents only by grant (`share` → `fork_from`).
  Lifecycle verbs: `kernel_start`, `shutdown_agents`, `kernel_stop`
- the bundle targets in the scoreboard below. The browser frontend is not a
  native bundle: it is the ts kernel LIBRARY (`src/lib/ts`) booted by an app
  (the CLI's canvas), whose page a host serves generically via a `file` agent
  (weak binding — the host never names the frontend)
- 2 umbrella targets (`FantasticKernelEmbedded`,
  `FantasticKernelFull`) — Apple-app entry points
- 1 CLI executable (`fantastic`)
- 1100+ tests across substrate, bundles, parity harness, public API

## Tiers

| product | platforms | includes |
|---|---|---|
| `FantasticKernelEmbedded` | iOS, iPadOS, visionOS, tvOS, watchOS, sandboxed macOS | All bundles EXCEPT subprocess-using ones |
| `FantasticKernelFull` | macOS Pro (unsandboxed) | All bundles including `exec_bridge` (commands + PTY jobs) |

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
| kernel_state | `FantasticKernelState` | both | the ROOT loader + persistence (records, blobs, the record-owner guard; a `root` meta serves a sub-namespace, e.g. a browser app's records) |
| ram_store | `FantasticRamStore` | both | the MEMORY STORE in RAM — every message, fold refs, grants (anchor cascade), search, inject/dump; one lock over all tables |
| static_loader | `FantasticStaticLoader` | both | declarative YAML roster loader — hydrates the declared agents as operator at boot |
| file_bridge | `FantasticFile` | both | the gated fs edge — sealed by default, deny-all `allow` list, running-dir clamp; text verbs read/write/edit/append/undo (version/if_match) + read_stream/write_stream/pump (raw bytes) |
| scheduler | `FantasticScheduler` | both | recurring tasks (schedule/unschedule/list/pause/resume/tick_now/history; `interval_seconds`); persists `schedules.yaml`/`schedule_events.yaml` THROUGH the loader (`persist_blob`/`load_blob`); failfast when no store is wired |
| cli | `FantasticCliBundle` | both | terminal render sink (`cli.tools`): token/done/say/error/status/context + the PTY intro to stdout; unknown verbs dropped |
| ws_bridge / relay_connector | `FantasticKernelBridge` + `FantasticIoBridge` | both | cross-kernel comms, io_bridge derivations (sealed by default); frames carry a qualified `caller` (`<guid>|<agent>`), ingress refuses the local guid. `relay_connector` dials a relay-KERNEL router (`ws://host/<guid>`, `X-Fantastic-Auth`), reaches a `partner_guid` and tunnels io_bridge frames |
| web | `FantasticWeb` | both | HTTP + WS server (swift-nio — serves on macOS + Linux) |
| web_ws | `FantasticWebWS` | both | composable WS verb surface (child of a `web` host; `web_ws.tools`) |
| web_rest | `FantasticWebRest` | both | composable REST verb surface (child of a `web` host; `web_rest.tools`) |
| ollama_backend | `FantasticOllamaBackend` | both | local LLM (thin over `FantasticAICore`) |
| nvidia_nim_backend | `FantasticNvidiaNimBackend` | both | hosted LLM, OpenAI-compatible, bearer auth + 429 retry (thin over `FantasticAICore`) |
| llamacpp_backend | `FantasticLlamaCppBackend` | both | local LLM via llama-server, OpenAI-compatible (thin over `FantasticAICore`) |
| vllm_backend | `FantasticVllmBackend` | both | LLM on a vLLM server, spoken to directly; a KV-cache session table per endpoint (thin over `FantasticAICore`) |
| anthropic_backend | `FantasticAnthropicBackend` | both | Claude via the Anthropic Messages API (thin over `FantasticAICore`) |
| exec_bridge | `FantasticExecBridge` | Pro | macOS-only — the process edge: a general-purpose command runner gated by a regex allowlist (`allowed`), sealed by default; jobs are kernel tasks; a terminal is a PTY job (`forkpty`) |
| scenario probes | `FantasticScenarioProbes` | test | fixture agents the shared e2e scenarios name |

## Wire compatibility

Verb names stay snake_case strings in JSON. Payload shapes match the
Python kernel byte-for-byte. The `kernel_bridge` bundle's in-process
transport pairs two kernels (e.g. Swift ↔ Python) for end-to-end
mixed-runtime tests.

`OrderedDictionary` (from swift-collections) backs the `JSON.object`
variant so `agent.yaml` byte-for-byte parity holds across runtimes (the
YAML emitter is `FantasticKernel/YamlIO.swift`, by hand; Yams parses).

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
    FantasticWeb/                      HTTP + WS server (swift-nio; macOS + Linux)
    FantasticKernelState/              the root loader + persistence
    FantasticRamStore/                 the memory store (in RAM)
    FantasticAICore/                   shared LLM machinery (memory-store conversation,
                                       tag grammar, folds, grants); AI backends are thin over this
    Fantastic{Ollama,NvidiaNim,LlamaCpp,Anthropic,Responses,Vllm}Backend/
                                       the AI backends
    Fantastic{File,Scheduler,StaticLoader,
              CliBundle,IoBridge,KernelBridge,WebWS,WebRest}/
                                       supporting bundles
    Fantastic{Terminal,Exec}{Backend,Bridge}/
                                       macOS-only Pro-tier bundles
    FantasticScenarioProbes/           e2e fixture agents (test)
    Fantastic/                         `fantastic` CLI executable
    FantasticKernelEmbedded/           Apple-app umbrella target (Lite tier)
    FantasticKernelFull/                Apple-app umbrella target (Pro tier, macOS-only consumer)
  Tests/
    Fantastic*Tests/                   per-target unit suites
    FantasticParityTests/              cross-runtime byte-diff harness
```

## Public API contract

The embedding surface:
- `Kernel.send([Item]) -> [Outcome]` — the ONE send; a single call is a
  one-element array
- `Kernel.subscribe { event in }` — state events, closure not callback protocol
- `saveJSON()` / `loadJSON(_:)` / `shutdown()`, `httpPort()`, `serial` (a
  process-unique id for per-kernel side tables)
- HTTP routes baked into `web`: `/_assets/favicon.png`, `/<agent_id>/file/<path>`;
  verb routes are declared by `web_ws` / `web_rest` sub-agents
- Bundle names: `file_bridge.tools`, `cli.tools`,
  `web.tools`, etc.

## Third-party dependencies

- [swift-collections](https://github.com/apple/swift-collections) 1.1+
  — `OrderedDictionary` for `JSON.object` key-order preservation
- [Yams](https://github.com/jpsim/Yams) 5.1+
  — the YAML parser for every kernel-written file (records, sidecars; `YamlIO`)
- [swift-nio](https://github.com/apple/swift-nio) 2.x
  — the cross-platform event-driven networking core; HTTP + WS server
  (`FantasticWeb`) and the relay/WS bridge clients run on it, so the kernel
  serves on macOS **and** Linux. No Hummingbird, no Vapor.

LLM backends run on `URLSession.AsyncBytes`.

## License

AGPL-3.0-or-later, same as the rest of the project. See `../LICENSE`.

---

*Part of **Aisixteen Fantastic** — licensed **AGPL-3.0-or-later** ([`../LICENSE`](../LICENSE)). "Aisixteen Fantastic" and "AISIXTEEN" (USPTO reg. 7,238,635) are trademarks of AISixteen; the license covers the code only, not the marks — forks must rename. See the [root README](../README.md#license--brand).*
