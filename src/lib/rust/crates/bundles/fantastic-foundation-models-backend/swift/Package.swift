// swift-tools-version: 6.0
//
// The Apple Foundation Models core and its C ABI, for the rust
// `foundation_models_backend` (the crate this directory lives in).
//
//   • FantasticFMCore    — THE ONE implementation of the on-device
//                          FoundationModels calls (gated inside by
//                          `#if canImport(FoundationModels) && os(macOS)`,
//                          macOS 26+ at run time; every other platform compiles
//                          and refuses at use).
//   • FantasticFMBridge  — the C ABI over the core (`fm_bridge.h`), built as the
//                          STATIC library `libFantasticFMBridge.a` that the
//                          crate's `build.rs` links.
//
// No dependencies: nothing here reaches outside this package.

import PackageDescription

let package = Package(
    name: "FantasticFoundationModels",
    platforms: [
        .macOS(.v14)
    ],
    products: [
        .library(name: "FantasticFMBridge", type: .static, targets: ["FantasticFMBridge"])
    ],
    targets: [
        .target(name: "FantasticFMCore"),
        .target(
            name: "FantasticFMBridge",
            dependencies: ["FantasticFMCore"],
            // The C header is the contract's documentation for the rust side; a
            // Swift target compiles none of it.
            exclude: ["fm_bridge.h"]
        ),
        // The core, the bridge's own `@_cdecl` functions, all against a FAKE
        // engine. No model, no macOS 26.
        .testTarget(
            name: "FantasticFMTests",
            dependencies: ["FantasticFMCore", "FantasticFMBridge"]
        ),
    ]
)
