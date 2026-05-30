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
//   • FantasticKernel + FantasticJSON + FantasticBundles
//     + FantasticKernelStartup        — granular libraries
//   • fantastic                       — CLI executable
//
// FantasticKernelEmbedded is the multi-platform sandbox-safe tier
// (iOS, iPadOS, visionOS, tvOS, watchOS, sandboxed macOS).
// FantasticKernelFull is the macOS-only Pro tier and additionally
// pulls in the subprocess-using bundles (terminal_backend,
// local_runner, python_runtime, ssh_runner) which are gated by
// `#if os(macOS)` inside the kernel itself.

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
        .library(name: "FantasticBundles", targets: [
            "FantasticFile",
            "FantasticYamlState",
            "FantasticProxyAgent",
            "FantasticTools",
            "FantasticHtmlAgent",
            "FantasticGlAgent",
            "FantasticScheduler",
            "FantasticCanvasBackend",
            "FantasticCanvasWebapp",
            "FantasticAiChatWebapp",
            "FantasticTerminalWebapp",
            "FantasticTelemetryPane",
            "FantasticCliBundle",
            "FantasticKernelBridge",
            "FantasticWeb",
            "FantasticWebWS",
            "FantasticWebRest",
            "FantasticOllamaBackend",
            "FantasticNvidiaNimBackend",
            "FantasticFoundationModelsBackend",
        ]),
        .library(name: "FantasticKernelStartup", targets: ["FantasticKernelStartup"]),

        // Apple-app-facing umbrella products. Both `@_exported import`
        // the kernel public surface; the tier split is realized by
        // which product the consuming app target depends on (Lite vs
        // Pro) + the `#if os(macOS)` gates inside the kernel.
        .library(name: "FantasticKernelEmbedded", targets: ["FantasticKernelEmbedded"]),
        .library(name: "FantasticKernelFull", targets: ["FantasticKernelFull"]),

        .executable(name: "fantastic", targets: ["Fantastic"]),
    ],
    dependencies: [
        .package(url: "https://github.com/apple/swift-collections.git", from: "1.1.0"),
        .package(url: "https://github.com/jpsim/Yams.git", from: "5.1.0"),
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
            ],
            resources: [.copy("Resources/root_readme.md")]
        ),
        .testTarget(
            name: "FantasticKernelTests",
            dependencies: ["FantasticKernel", "FantasticJSON"]
        ),

        // ── Trivially-portable bundles (Phase 3) ─────────────────
        .target(
            name: "FantasticFile",
            dependencies: ["FantasticKernel", "FantasticJSON",
                           .product(name: "OrderedCollections", package: "swift-collections")]
        ),
        .target(
            name: "FantasticYamlState",
            dependencies: ["FantasticKernel", "FantasticJSON",
                           .product(name: "OrderedCollections", package: "swift-collections"),
                           .product(name: "Yams", package: "Yams")]
        ),
        .target(
            name: "FantasticProxyAgent",
            dependencies: ["FantasticKernel", "FantasticJSON",
                           .product(name: "OrderedCollections", package: "swift-collections")]
        ),
        .target(
            name: "FantasticTools",
            dependencies: ["FantasticKernel", "FantasticJSON",
                           .product(name: "OrderedCollections", package: "swift-collections")]
        ),
        .target(
            name: "FantasticHtmlAgent",
            dependencies: ["FantasticKernel", "FantasticJSON"]
        ),
        .target(
            name: "FantasticGlAgent",
            dependencies: ["FantasticKernel", "FantasticJSON"]
        ),
        .target(
            name: "FantasticScheduler",
            dependencies: ["FantasticKernel", "FantasticJSON"]
        ),
        .target(
            name: "FantasticCanvasBackend",
            dependencies: ["FantasticKernel", "FantasticJSON",
                           .product(name: "OrderedCollections", package: "swift-collections")]
        ),
        .target(
            name: "FantasticCanvasWebapp",
            dependencies: ["FantasticKernel", "FantasticJSON"],
            resources: [.copy("Resources/canvas.html")]
        ),
        .target(
            name: "FantasticAiChatWebapp",
            dependencies: ["FantasticKernel", "FantasticJSON"]
        ),
        .target(
            name: "FantasticTerminalWebapp",
            dependencies: ["FantasticKernel", "FantasticJSON"],
            resources: [.copy("Resources/index.html")]
        ),
        .target(
            name: "FantasticTelemetryPane",
            dependencies: ["FantasticKernel", "FantasticJSON"]
        ),
        .target(
            name: "FantasticCliBundle",
            dependencies: ["FantasticKernel", "FantasticJSON"]
        ),
        .target(
            name: "FantasticKernelBridge",
            dependencies: ["FantasticKernel", "FantasticJSON"]
        ),
        .target(
            name: "FantasticWeb",
            dependencies: ["FantasticKernel", "FantasticJSON"],
            resources: [
                .copy("Resources/three.module.js"),
                .copy("Resources/xterm.min.js"),
                .copy("Resources/xterm.min.css"),
                .copy("Resources/xterm-addon-fit.min.js"),
                .copy("Resources/transport.js"),
            ]
        ),
        // Composable web surfaces (children of a `web` host). They run
        // no server — they return `get_routes` descriptors the host
        // mounts. WS handling is the host's shared proxy, so neither
        // depends on FantasticWeb (no cycle).
        .target(
            name: "FantasticWebWS",
            dependencies: ["FantasticKernel", "FantasticJSON"]
        ),
        .target(
            name: "FantasticWebRest",
            dependencies: [
                "FantasticKernel", "FantasticJSON",
                .product(name: "OrderedCollections", package: "swift-collections"),
            ]
        ),

        // ── LLM backends (Phase 5 / 8D) ──────────────────────────
        .target(
            name: "FantasticOllamaBackend",
            dependencies: ["FantasticKernel", "FantasticJSON",
                           .product(name: "OrderedCollections", package: "swift-collections")]
        ),
        .target(
            name: "FantasticNvidiaNimBackend",
            dependencies: ["FantasticKernel", "FantasticJSON",
                           .product(name: "OrderedCollections", package: "swift-collections")]
        ),
        .target(
            name: "FantasticFoundationModelsBackend",
            dependencies: ["FantasticKernel", "FantasticJSON",
                           .product(name: "OrderedCollections", package: "swift-collections")]
        ),

        // ── Pro-tier subprocess bundles (macOS only) ─────────────
        .target(
            name: "FantasticLocalRunner",
            dependencies: ["FantasticKernel", "FantasticJSON"]
        ),
        .target(
            name: "FantasticPythonRuntime",
            dependencies: ["FantasticKernel", "FantasticJSON"]
        ),
        .target(
            name: "FantasticSshRunner",
            dependencies: ["FantasticKernel", "FantasticJSON"]
        ),
        .target(
            name: "FantasticTerminalBackend",
            dependencies: ["FantasticKernel", "FantasticJSON"]
        ),

        // ── Startup layer (Phase 8A) ─────────────────────────────
        // Composes the default bundle set + exposes
        // startKernelInMemory / startKernel free functions matching
        // the Rust UniFFI surface. App-facing entry point.
        .target(
            name: "FantasticKernelStartup",
            dependencies: [
                "FantasticKernel", "FantasticJSON",
                "FantasticFile", "FantasticProxyAgent", "FantasticTools",
                "FantasticHtmlAgent", "FantasticGlAgent", "FantasticScheduler",
                "FantasticCanvasBackend", "FantasticCanvasWebapp",
                "FantasticAiChatWebapp", "FantasticTerminalWebapp",
                "FantasticTelemetryPane", "FantasticCliBundle",
                "FantasticKernelBridge", "FantasticWeb", "FantasticWebWS", "FantasticWebRest",
                "FantasticYamlState",
                "FantasticOllamaBackend", "FantasticNvidiaNimBackend",
                "FantasticFoundationModelsBackend",
                "FantasticLocalRunner", "FantasticPythonRuntime",
                "FantasticSshRunner", "FantasticTerminalBackend",
            ]
        ),
        .testTarget(
            name: "FantasticKernelStartupTests",
            dependencies: [
                "FantasticKernelStartup", "FantasticKernel", "FantasticJSON",
                "FantasticProxyAgent",
            ]
        ),
        // ── Cross-runtime parity (Phase 8J) ──────────────────────
        // Spawns the Rust `fantastic` binary as a subprocess +
        // fires identical verb payloads at both kernels; diffs the
        // JSON replies. Skips when RUST_KERNEL_BIN env var is unset.
        .testTarget(
            name: "FantasticParityTests",
            dependencies: ["FantasticKernel", "FantasticJSON", "FantasticKernelStartup"]
        ),

        // ── Apple-app umbrella targets ───────────────────────────
        // `@_exported import` the kernel public surface so app code
        // sees `Kernel`, `ProxyAgent`, `startKernelInMemory`, etc.
        // through a stable product name. Embedded skips Pro-tier
        // bundle deps so it doesn't drag subprocess-using code into
        // iOS/sandboxed builds even though those bundles compile to
        // empty on non-macOS — keeps the dep graph honest.
        .target(
            name: "FantasticKernelEmbedded",
            dependencies: [
                "FantasticJSON", "FantasticKernel",
                "FantasticKernelStartup",
                "FantasticProxyAgent", "FantasticTools",
            ]
        ),
        .target(
            name: "FantasticKernelFull",
            dependencies: [
                "FantasticJSON", "FantasticKernel",
                "FantasticKernelStartup",
                "FantasticProxyAgent", "FantasticTools",
                "FantasticLocalRunner", "FantasticPythonRuntime",
                "FantasticSshRunner", "FantasticTerminalBackend",
            ]
        ),

        // ── CLI executable (Phase 7) ─────────────────────────────
        .executableTarget(
            name: "Fantastic",
            dependencies: [
                "FantasticKernelStartup", "FantasticKernel", "FantasticJSON",
            ]
        ),
        .testTarget(
            name: "FantasticCLITests",
            dependencies: ["Fantastic", "FantasticJSON", "FantasticKernel"]
        ),

        .testTarget(
            name: "FantasticBundlesTests",
            dependencies: [
                "FantasticKernel", "FantasticJSON",
                "FantasticFile", "FantasticProxyAgent", "FantasticTools",
                "FantasticHtmlAgent", "FantasticGlAgent", "FantasticScheduler",
                "FantasticCanvasBackend", "FantasticCanvasWebapp",
                "FantasticAiChatWebapp", "FantasticTerminalWebapp",
                "FantasticTelemetryPane", "FantasticCliBundle",
                "FantasticKernelBridge", "FantasticWeb", "FantasticWebWS", "FantasticWebRest",
                "FantasticYamlState", "FantasticKernelStartup",
                "FantasticFoundationModelsBackend",
            ]
        ),
    ]
)
