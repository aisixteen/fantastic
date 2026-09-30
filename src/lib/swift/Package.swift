// swift-tools-version: 6.0
//
// Fantastic Kernel — Swift workspace.
//
// Native kernel for Apple-platform deployment. The reference
// implementation for non-Apple deployments is the Python kernel in
// `../python/`; both share on-disk and wire format byte-for-byte.
//
// Public products:
//   • FantasticKernel{Embedded,Full}  — umbrella modules the Apple
//                                       app imports (`@_exported`)
//   • FantasticKernel + FantasticJSON + FantasticIoBridge
//     + FantasticBundles + FantasticKernelStartup
//                                      — granular libraries
//     (FantasticIoBridge is exported so external hosts — e.g. the
//      fantastic_relay router — reuse the canonical IngressRules /
//      egress rules instead of a local copy)
//   • fantastic                       — CLI executable
//
// FantasticKernelEmbedded is the multi-platform sandbox-safe tier
// (iOS, iPadOS, visionOS, tvOS, watchOS, sandboxed macOS).
// FantasticKernelFull is the macOS-only Pro tier and additionally
// pulls in the subprocess-using bundle (exec_bridge — commands and PTY
// jobs, the one spawner) which is gated by `#if os(macOS)` inside the
// kernel itself.

import PackageDescription

let package = Package(
    name: "FantasticKernel",
    platforms: [
        .macOS(.v14),
        .iOS(.v17),
        .visionOS(.v1),
        .tvOS(.v17),
        .watchOS(.v10),
    ],
    products: [
        .library(name: "FantasticJSON", targets: ["FantasticJSON"]),
        .library(name: "FantasticKernel", targets: ["FantasticKernel"]),
        // The io base (channel model + IngressRules/egress rule registries +
        // gate_inbound). Exported so external hosts (the fantastic_relay router)
        // delegate their password rule to the canonical rules, not a local copy.
        .library(name: "FantasticIoBridge", targets: ["FantasticIoBridge"]),
        .library(name: "FantasticBundles", targets: [
            "FantasticKernelState",
            "FantasticFile",
            "FantasticScheduler",
            "FantasticCliBundle",
            "FantasticKernelBridge",
            "FantasticWeb",
            "FantasticWebWS",
            "FantasticWebRest",
            "FantasticOllamaBackend",
            "FantasticNvidiaNimBackend",
            "FantasticAnthropicBackend",
            "FantasticResponsesBackend",
            "FantasticLlamaCppBackend",
            "FantasticVllmBackend",
            "FantasticRamStore",
        ]),
        .library(name: "FantasticKernelStartup", targets: ["FantasticKernelStartup"]),

        // Apple-app-facing umbrella products. Both `@_exported import`
        // the kernel public surface; the tier split is realized by
        // which product the consuming app target depends on (Lite vs
        // Pro) + the `#if os(macOS)` gates inside the kernel.
        .library(name: "FantasticKernelEmbedded", targets: ["FantasticKernelEmbedded"]),
        .library(name: "FantasticKernelFull", targets: ["FantasticKernelFull"]),

        .executable(name: "fantastic_kernel", targets: ["Fantastic"]),
    ],
    dependencies: [
        .package(url: "https://github.com/apple/swift-collections.git", from: "1.1.0"),
        // Yams: the YAML PARSER for every kernel-written file (`YamlIO`);
        // pinned EXACT — the on-disk bytes are a cross-runtime contract.
        .package(url: "https://github.com/jpsim/Yams.git", exact: "5.4.0"),
        // swift-nio: the cross-platform (macOS + Linux) event-driven networking
        // core — the `FantasticWeb` HTTP + WS server AND the relay/WS bridge
        // clients run on it. swift-nio-ssl vendors BoringSSL for TLS on the
        // wss:// dials (the only vetted TLS-1.3-over-buffers path in Swift).
        .package(url: "https://github.com/apple/swift-nio.git", from: "2.65.0"),
        .package(url: "https://github.com/apple/swift-nio-ssl.git", from: "2.27.0"),
        // Cross-platform (macOS + Linux) streaming HTTP client for the LLM
        // backends — replaces URLSession.bytes (Apple-only) so ollama/nvidia
        // stream on Linux too. NIO-based, shares the swift-nio above.
        .package(url: "https://github.com/swift-server/async-http-client.git", from: "1.20.0"),
    ],
    targets: [
        // ── Core ─────────────────────────────────────────────────
        .target(
            name: "FantasticJSON",
            dependencies: [.product(name: "OrderedCollections", package: "swift-collections")]
        ),
        .testTarget(name: "FantasticJSONTests", dependencies: ["FantasticJSON"]),

        .target(
            name: "FantasticKernel",
            dependencies: [
                "FantasticJSON",
                .product(name: "OrderedCollections", package: "swift-collections"),
                .product(name: "Yams", package: "Yams"),
            ],
            resources: [.copy("Resources/root_readme.md"), .copy("Resources/docs")]
        ),
        .testTarget(
            name: "FantasticKernelTests",
            dependencies: ["FantasticKernel", "FantasticJSON", "FantasticKernelState",
                           "FantasticRamStore"]
        ),

        // ── Trivially-portable bundles (Phase 3) ─────────────────
        // The IO base — the shared auth/gate/codec library (mirror of py
        // `io/io_bridge` + rust `fantastic-io-bridge`). Every io derivation
        // (file_bridge / web_ws / web_rest / the bridges) imports it.
        .target(
            name: "FantasticIoBridge",
            dependencies: [
                "FantasticKernel", "FantasticJSON",
                .product(name: "OrderedCollections", package: "swift-collections"),
                // The web-route seam hands a websocket endpoint over as the NIO
                // handler the host installs on the upgraded channel.
                .product(name: "NIOCore", package: "swift-nio"),
            ]
        ),
        .target(
            name: "FantasticFile",
            dependencies: ["FantasticKernel", "FantasticJSON", "FantasticIoBridge",
                           .product(name: "OrderedCollections", package: "swift-collections")]
        ),
        // The `kernel_state.tools` ROOT LOADER — the durable-state bundle the
        // tree root carries. Its verb surface only; the boot machinery lives in
        // the kernel (`RecordWriter.swift`), which the kernel's own tests drive.
        .target(
            name: "FantasticKernelState",
            dependencies: ["FantasticKernel", "FantasticJSON",
                           .product(name: "OrderedCollections", package: "swift-collections")]
        ),
        // The MEMORY STORE in RAM (python `memory/ram_store`): rows per owner,
        // fold refs, grants with FK cascade, a text index; inject/dump. The root's
        // `memory_store_id` and every AI agent's name one.
        .target(
            name: "FantasticRamStore",
            dependencies: ["FantasticKernel", "FantasticJSON",
                           .product(name: "OrderedCollections", package: "swift-collections")]
        ),
        .target(
            name: "FantasticScheduler",
            // OrderedCollections: `validateMeta`'s signature names
            // `OrderedDictionary<String, JSON>` (the record shape).
            dependencies: ["FantasticKernel", "FantasticJSON",
                           .product(name: "OrderedCollections", package: "swift-collections")]
        ),
        .target(
            name: "FantasticStaticLoader",
            dependencies: [
                "FantasticKernel", "FantasticJSON",
                .product(name: "OrderedCollections", package: "swift-collections"),
                .product(name: "Yams", package: "Yams"),
                "FantasticExecBridge",
                // The running-dir clamp: `fs` is the one disk surface, and the
                // cold manifest read is not exempt (python and rust do the same).
                "FantasticFile",
            ]
        ),
        .target(
            name: "FantasticCliBundle",
            dependencies: ["FantasticKernel", "FantasticJSON"]
        ),
        .target(
            name: "FantasticKernelBridge",
            dependencies: [
                "FantasticKernel", "FantasticJSON", "FantasticIoBridge",
                // Reflect and the record meta are ORDERED maps: python's key
                // order is the contract, so the engine cannot use a Dictionary.
                .product(name: "OrderedCollections", package: "swift-collections"),
                .product(name: "NIOCore", package: "swift-nio"),
                .product(name: "NIOPosix", package: "swift-nio"),
                // HTTP/1 + WebSocket: the cross-platform WS CLIENT
                // (NIOWebSocketClient) the bridge dials peers/relay with —
                // replaces URLSessionWebSocketTask (Apple-only, dead on Linux).
                .product(name: "NIOHTTP1", package: "swift-nio"),
                .product(name: "NIOWebSocket", package: "swift-nio"),
                // NIOSSL: TLS for the WS client's wss:// dials (relay via cloudflared).
                .product(name: "NIOSSL", package: "swift-nio-ssl"),
            ]
        ),
        .target(
            name: "FantasticWeb",
            dependencies: [
                "FantasticKernel", "FantasticJSON", "FantasticIoBridge",
                // swift-nio HTTP + WebSocket server (cross-platform: macOS + Linux).
                // Replaces the Apple-only Network.framework listener so the swift
                // kernel serves HTTP on Linux too. All from the already-declared
                // swift-nio package — no new top-level dependency.
                .product(name: "NIOCore", package: "swift-nio"),
                .product(name: "NIOPosix", package: "swift-nio"),
                .product(name: "NIOHTTP1", package: "swift-nio"),
                .product(name: "NIOWebSocket", package: "swift-nio"),
                .product(name: "NIOFoundationCompat", package: "swift-nio"),
            ],
            resources: [
                // python `web/favicon.png`, byte for byte — the only static
                // bytes the host serves (it renders nothing of its own).
                .copy("Resources/favicon.png"),
            ]
        ),
        // Composable web surfaces (children of a `web` host). They run
        // no server — they DECLARE routes (`get_routes`) and hand the host the
        // live endpoint per path (`WebRouteEndpoints`, python
        // `route_endpoints`). The WS proxy is web_ws's own endpoint (python
        // `web_ws/_proxy.py`), so neither depends on FantasticWeb (no cycle).
        .target(
            name: "FantasticWebWS",
            dependencies: [
                "FantasticKernel", "FantasticJSON", "FantasticIoBridge",
                .product(name: "OrderedCollections", package: "swift-collections"),
                .product(name: "NIOCore", package: "swift-nio"),
                .product(name: "NIOWebSocket", package: "swift-nio"),
                .product(name: "NIOFoundationCompat", package: "swift-nio"),
            ]
        ),
        .target(
            name: "FantasticWebRest",
            dependencies: [
                "FantasticKernel", "FantasticJSON", "FantasticIoBridge",
                .product(name: "OrderedCollections", package: "swift-collections"),
            ]
        ),

        // ── LLM backends (Phase 5 / 8D) ──────────────────────────
        // Shared reflect-driven agent machinery behind an `AIProvider`
        // seam — the backends below supply only their provider +
        // an `AIBackendConfig`. MUST NOT import any provider SDK.
        .target(
            name: "FantasticAICore",
            dependencies: ["FantasticKernel", "FantasticJSON",
                           .product(name: "OrderedCollections", package: "swift-collections"),
                           // ByteBuffer for the cross-platform line-streaming helper.
                           .product(name: "NIOCore", package: "swift-nio")]
        ),
        .target(
            name: "FantasticOllamaBackend",
            dependencies: ["FantasticAICore", "FantasticKernel", "FantasticJSON",
                           .product(name: "OrderedCollections", package: "swift-collections"),
                           .product(name: "AsyncHTTPClient", package: "async-http-client"),
                           .product(name: "NIOCore", package: "swift-nio")]
        ),
        .target(
            name: "FantasticNvidiaNimBackend",
            dependencies: ["FantasticAICore", "FantasticKernel", "FantasticJSON",
                           .product(name: "OrderedCollections", package: "swift-collections"),
                           .product(name: "AsyncHTTPClient", package: "async-http-client"),
                           .product(name: "NIOCore", package: "swift-nio")]
        ),
        // TEST-ONLY fixture agents for the shared e2e scenarios. Registration
        // is behind `-DSCENARIO_PROBES`, which only the e2e driver builds with.
        .target(
            name: "FantasticScenarioProbes",
            dependencies: ["FantasticKernel", "FantasticJSON"]
        ),
        .target(
            name: "FantasticLlamaCppBackend",
            dependencies: ["FantasticAICore", "FantasticKernel", "FantasticJSON",
                           .product(name: "OrderedCollections", package: "swift-collections"),
                           .product(name: "AsyncHTTPClient", package: "async-http-client"),
                           .product(name: "NIOCore", package: "swift-nio")]
        ),
        .target(
            name: "FantasticAnthropicBackend",
            dependencies: ["FantasticAICore", "FantasticKernel", "FantasticJSON",
                           .product(name: "OrderedCollections", package: "swift-collections"),
                           .product(name: "AsyncHTTPClient", package: "async-http-client"),
                           .product(name: "NIOCore", package: "swift-nio")]
        ),
        .target(
            name: "FantasticResponsesBackend",
            dependencies: ["FantasticAICore", "FantasticKernel", "FantasticJSON",
                           .product(name: "OrderedCollections", package: "swift-collections"),
                           .product(name: "AsyncHTTPClient", package: "async-http-client"),
                           .product(name: "NIOCore", package: "swift-nio")]
        ),
        // vLLM, spoken to DIRECTLY — its own knobs, its own failure shapes and a
        // KV-cache session table per endpoint (python `vllm_backend`).
        .target(
            name: "FantasticVllmBackend",
            dependencies: ["FantasticAICore", "FantasticKernel", "FantasticJSON",
                           .product(name: "OrderedCollections", package: "swift-collections"),
                           .product(name: "AsyncHTTPClient", package: "async-http-client"),
                           .product(name: "NIOCore", package: "swift-nio")]
        ),
        // ── Shared runner lifecycle (mirrors FantasticAICore) ────
        // Cross-platform dispatch skeleton (reflect/boot/shutdown/
        // start/stop) behind a `RunnerTransport` seam. The local + ssh
        // runner bundles supply only their transport conformance. NO
        // ── The process edge (macOS only) ────────────────────────
        .target(
            name: "FantasticExecBridge",
            dependencies: ["FantasticKernel", "FantasticIoBridge", "FantasticJSON", "FantasticFile"]
        ),

        // ── Startup layer (Phase 8A) ─────────────────────────────
        // Composes the default bundle set + exposes
        // startKernelInMemory / startKernel free functions matching
        // the Rust UniFFI surface. App-facing entry point.
        .target(
            name: "FantasticKernelStartup",
            dependencies: [
                // The root record's persisted meta is an ordered JSON object —
                // startup carries it onto the root agent verbatim.
                .product(name: "OrderedCollections", package: "swift-collections"),
                // TEST-ONLY fixtures; registration is behind -DSCENARIO_PROBES
                // so a default build carries none of it.
                "FantasticScenarioProbes",
                "FantasticKernel", "FantasticJSON",
                "FantasticKernelState",
                "FantasticFile",
                "FantasticScheduler", "FantasticStaticLoader", "FantasticCliBundle",
                "FantasticKernelBridge", "FantasticWeb", "FantasticWebWS", "FantasticWebRest",
                "FantasticRamStore",
                "FantasticOllamaBackend", "FantasticNvidiaNimBackend",
                "FantasticAnthropicBackend",
                "FantasticResponsesBackend",
                "FantasticLlamaCppBackend",
                "FantasticVllmBackend",
                "FantasticExecBridge",
            ]
        ),
        .testTarget(
            name: "FantasticKernelStartupTests",
            dependencies: [
                "FantasticKernelStartup", "FantasticKernel", "FantasticJSON",
            ]
        ),
        // The subprocess bundle's own suite — the ORPHAN gate: `cancel` must
        // reap the real OS child, asserted on the pid, not on the Swift Task.
        .testTarget(
            name: "FantasticExecBridgeTests",
            dependencies: ["FantasticExecBridge", "FantasticKernel", "FantasticJSON"]
        ),
        // ── Cross-runtime parity (Phase 8J) ──────────────────────
        // Spawns the Python kernel binary as a subprocess +
        // fires identical verb payloads at both kernels; diffs the
        // JSON replies. Skips when PYTHON_KERNEL_BIN env var is unset.
        .testTarget(
            name: "FantasticParityTests",
            dependencies: ["FantasticKernel", "FantasticJSON", "FantasticKernelStartup"]
        ),

        // ── Apple-app umbrella targets ───────────────────────────
        // `@_exported import` the kernel public surface so app code
        // sees `Kernel`, `startKernelInMemory`, etc.
        // through a stable product name. Embedded skips Pro-tier
        // bundle deps so it doesn't drag subprocess-using code into
        // iOS/sandboxed builds even though those bundles compile to
        // empty on non-macOS — keeps the dep graph honest.
        .target(
            name: "FantasticKernelEmbedded",
            dependencies: [
                "FantasticJSON", "FantasticKernel",
                "FantasticKernelStartup",
            ]
        ),
        .target(
            name: "FantasticKernelFull",
            dependencies: [
                "FantasticJSON", "FantasticKernel",
                "FantasticKernelStartup",
                "FantasticExecBridge",
            ]
        ),

        // ── CLI executable (Phase 7) ─────────────────────────────
        .executableTarget(
            name: "Fantastic",
            dependencies: [
                "FantasticKernelStartup", "FantasticKernel", "FantasticJSON",
                "FantasticKernelBridge",
                // `fantastic help` prints the `cli` bundle's help.md — the CLI
                // renders it, so the bytes live in the bundle (python reads the
                // same file out of the installed `cli` package).
                "FantasticCliBundle",
            ]
        ),
        .testTarget(
            name: "FantasticCLITests",
            dependencies: [
                "Fantastic", "FantasticJSON", "FantasticKernel", "FantasticCliBundle",
            ]
        ),

        .testTarget(
            name: "FantasticAICoreTests",
            dependencies: [
                "FantasticAICore", "FantasticKernel", "FantasticJSON", "FantasticRamStore",
                .product(name: "OrderedCollections", package: "swift-collections"),
            ]
        ),

        // Provider WIRE decoders — the ollama NDJSON / NIM SSE line→chunk
        // mapping, extracted as PURE functions so the two-channel reading is
        // testable without a live server. Reading a single channel is exactly
        // how these shipped broken (a reasoning model's whole turn read as an
        // empty completion), and it was untestable while the mapping sat inside
        // the HTTP closure.
        .testTarget(
            name: "FantasticAIBackendsTests",
            dependencies: [
                "FantasticAICore", "FantasticOllamaBackend", "FantasticNvidiaNimBackend",
                "FantasticAnthropicBackend",
                "FantasticResponsesBackend",
            "FantasticLlamaCppBackend", "FantasticVllmBackend", "FantasticJSON",
            ]
        ),

        .testTarget(
            name: "FantasticBundlesTests",
            dependencies: [
                "FantasticKernel", "FantasticJSON",
                "FantasticKernelState",
                "FantasticFile",
                "FantasticScheduler", "FantasticStaticLoader", "FantasticCliBundle",
                "FantasticKernelBridge", "FantasticWeb", "FantasticWebWS", "FantasticWebRest",
                "FantasticKernelStartup", "FantasticRamStore",
                // The scenario fixtures are unit-tested here; only their
                // REGISTRATION is behind -DSCENARIO_PROBES.
                "FantasticScenarioProbes",
                // The agentic runner boots the rosters the shared corpus at
                // `contract/agentic/` declares, so it needs the bundles those
                // manifests name — and Yams to read the corpus itself.
                .product(name: "Yams", package: "Yams"),
                "FantasticExecBridge",
                // The SCENARIO runner (ScenarioTests.swift) reads the shared
                // scenarios at `e2e/tests/scenario/` and drives a live model,
                // so it needs the anthropic backend and ordered maps.
                "FantasticAnthropicBackend",
                .product(name: "OrderedCollections", package: "swift-collections"),
                // A stub leg's websocket endpoint is a NIO handler (the web
                // route seam).
                .product(name: "NIOCore", package: "swift-nio"),
                .product(name: "NIOWebSocket", package: "swift-nio"),
            ]
        ),
    ]
)
