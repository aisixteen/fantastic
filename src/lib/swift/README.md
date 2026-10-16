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
  context moves between agents in the text of a call.
  Lifecycle verbs: `kernel_start`, `shutdown_agents`, `kernel_stop`
- the bundle targets in the scoreboard below. The browser frontend is not a
  native bundle: it is the ts kernel LIBRARY (`src/lib/ts`) booted by an app
  (the CLI's canvas), whose page a host serves generically via a `file` agent
  (weak binding — the host never names the frontend)
- 2 umbrella targets (`FantasticKernelEmbedded`,
  `FantasticKernelFull`) — Apple-app entry points
- 1 CLI executable (`fantastic`)
- 1600+ tests across substrate, bundles, parity harness, public API

## Tiers

| product | platforms | includes |
|---|---|---|
| `FantasticKernelEmbedded` | iOS, iPadOS, visionOS, tvOS, watchOS, sandboxed macOS | All bundles EXCEPT subprocess-using ones |
| `FantasticKernelFull` | macOS Pro (unsandboxed) | All bundles including `exec_bridge` (commands + PTY jobs) and `py_agent` (folder-bundle programs) |

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
| ram_store | `FantasticRamStore` | both | the MEMORY STORE in RAM — every message, keyed by its writer (the calling agent; the kernel's own rows as the operator's); fold refs (cascade), search, inject/dump; one lock over all tables |
| credential_store | `FantasticCredentialStore` | both | secrets by ref, each bound to one host — the call gate alone decides who reaches it; a `set` payload and a `get` reply are recorded like any call's, in clear (owner, 2026-10-04); `file` at an ABSOLUTE path (`/…` or `~…`, a relative one refused at the write; 0600, read only if this user's and no group/other bits), OS `keychain` (Security.framework, items under `keychain_namespace`, default `fantastic`; fails loud where there is none) or read-only `env` backend. A ref is declared ONLY by `update_agent` on the store's `refs` (`{host, fields?/env?}`); `set` stores a declared ref's value, never creates one ("no such ref", the operator included) and takes no `host` (the binding is the declaration's); `forget` drops the value and keeps the declaration. The file is judged on the ONE open it is read from (`O_NOFOLLOW` + `fstat`, `fsReadPrivate`). Every keyed AI backend reads its key here (`credentials: {store, ref}`), before each request, as itself (its own turn) |
| static_loader | `FantasticStaticLoader` | both | declarative YAML roster loader — hydrates the declared agents as operator at boot |
| file_bridge | `FantasticFile` | both | the gated fs edge — sealed by default, deny-all `allow` list, running-dir clamp; text verbs read/write/edit/append/undo (version/if_match) + read_stream/write_stream/pump (raw bytes; both pump ends run AS THE BRIDGE itself) |
| scheduler | `FantasticScheduler` | both | recurring tasks (schedule/unschedule/list/pause/resume/tick_now/history; `interval_seconds`); persists `schedules.yaml`/`schedule_events.yaml` THROUGH the loader (`persist_blob`/`load_blob`); failfast when no store is wired. A schedule fires AS THE SCHEDULER (its own `can_call`); every caller sees every schedule |
| cli | `FantasticCliBundle` | both | terminal render sink (`cli.tools`): token/done/say/error/status/context + the PTY intro to stdout; unknown verbs dropped |
| ws_bridge / relay_connector | `FantasticKernelBridge` + `FantasticIoBridge` | both | cross-kernel comms, io_bridge derivations (sealed by default); a call the leg serves is the leg's own send (its read loop is a task its `boot` spawned), gated as THE LEG — its `can_call`, the target's `callable_by`. A leg's rules are read off the LIVE record per frame (a re-seal or an egress change applies to the next frame); frame ids are `<leg>-<boot nonce>-<n>`, so a restarted sender never joins a settled execution. `relay_connector` dials a relay-KERNEL router (`ws://host/<guid>`, `X-Fantastic-Auth` — the password read from a credential_store, `credentials: {store, ref}` field `password`, before every dial; never a record field), reaches a `partner_guid` and tunnels io_bridge frames |
| web | `FantasticWeb` | both | HTTP + WS server (swift-nio — serves on macOS + Linux) |
| web_ws | `FantasticWebWS` | both | composable WS verb surface (child of a `web` host; `web_ws.tools`); every admitted frame is handed to the leg (`inbound_call`/`inbound_emit`/`inbound_watch`/`inbound_state_subscribe`), which performs it AS ITSELF — gated as the leg |
| web_rest | `FantasticWebRest` | both | composable REST verb surface (child of a `web` host; `web_rest.tools`); every admitted request is handed to the leg (`inbound_call`, JSON and octet alike), which sends it AS ITSELF; a POST is gated on its head (target + `X-Fantastic-Auth`) before its body is read, and a body over 16 MiB (io_bridge `MAX_FRAME`) is refused 413 |
| ollama_backend | `FantasticOllamaBackend` | both | local LLM (thin over `FantasticAICore`) |
| responses_backend | `FantasticResponsesBackend` | both | OpenAI Responses-API LLM (thin over `FantasticAICore`) |
| vllm_backend | `FantasticVllmBackend` | both | LLM on a vLLM server, spoken to directly; a KV-cache session table per endpoint (thin over `FantasticAICore`) |
| anthropic_backend | `FantasticAnthropicBackend` | both | Anthropic Messages-API LLM — streamed, no native tools; the key from a credential_store ref, sent as `x-api-key` (thin over `FantasticAICore`) |
| mcp_client | `FantasticMcpClient` | both | ONE remote MCP server as an agent (`mcp_client.tools`) — each server tool is a verb of the agent, documented per instance; static key from a credential_store ref |
| foundation_models_backend | `FantasticFoundationModelsBackend` + `FantasticFMCore` | both | Apple's on-device model (macOS 26+ with Apple Intelligence; anywhere else a send fails naming why); no endpoint, model or key; `context_window` defaults to the SDK's real window (4096 measured on macOS 27.0). `FantasticFMCore` is the ONE implementation of the FoundationModels calls — rust links it through `FantasticFMBridge`, a static C ABI (`fm_bridge.h`) (thin over `FantasticAICore`) |
| exec_bridge | `FantasticExecBridge` | Pro | macOS-only — the process edge: a general-purpose command runner gated by a regex allowlist (`allowed`), sealed by default; jobs are kernel tasks; a terminal is a PTY job (`forkpty`); any caller the call gate admits drives any job of the agent |
| py_agent | `FantasticExecBridge` | Pro | macOS-only — the base a folder bundle (`bundles/<name>/bundle.yaml`, `base: py_agent`) extends: runs the folder's program per instance over the exec_bridge relay, reached only through its declared verbs |
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
FM_LIVE=1 swift test \
    --filter liveOnDeviceModel          # the on-device model, live (opt-in)
swift build -c release --product FantasticFMBridge
                                        # .build/release/libFantasticFMBridge.a (rust links it)
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
    FantasticCredentialStore/          secrets by ref (file / keychain / env)
    FantasticAICore/                   shared LLM machinery (memory-store conversation,
                                       tag grammar, folds, grants); AI backends are thin over this
    Fantastic{Ollama,Responses,Vllm,Anthropic}Backend/
                                       the AI backends
    FantasticFMCore/                   Apple Foundation Models calls (the one implementation)
    FantasticFMBridge/                 its C ABI for rust — static lib + fm_bridge.h
    FantasticFoundationModelsBackend/  the on-device AI backend (+ Resources/readme.md)
    Fantastic{File,Scheduler,StaticLoader,
              CliBundle,IoBridge,KernelBridge,WebWS,WebRest,McpClient}/
                                       supporting bundles
    FantasticExecBridge/               macOS-only Pro-tier bundles (exec_bridge, py_agent)
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
