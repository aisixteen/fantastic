// swift-tools-version:5.9
//
// FantasticKernelFull — desktop/PTY tier of the Rust kernel as a Swift
// Package. Wraps `Fantastic-Full.xcframework` (Mac-only).
//
// Use this from FantasticPro (macOS, Developer ID, unsandboxed). The
// full tier registers PTY-using bundles (terminal_backend, local_runner,
// python_runtime, ssh_runner once ported) — bundles the iOS sandbox
// forbids. There are no iOS slices in this XCFramework; targeting iOS
// from this product is a build-time error by design.
//
// Lite uses the sister package `FantasticKernelEmbedded`.
//
// Consume from project.yml:
//   packages:
//     FantasticKernelFull:
//       path: ../../fantastic_canvas/rust/packaging/FantasticKernelFull
//   targets:
//     FantasticPro:
//       dependencies:
//         - package: FantasticKernelFull
//           product: FantasticKernelFull
//
// The XCFramework is built by `rust/scripts/build-xcframework-full.sh`.

import PackageDescription

let package = Package(
    name: "FantasticKernelFull",
    platforms: [
        .macOS(.v13),
    ],
    products: [
        .library(
            name: "FantasticKernelFull",
            targets: ["FantasticKernelFull"]
        ),
    ],
    targets: [
        // The Swift wrapper layer (re-exports the UniFFI-generated bindings).
        .target(
            name: "FantasticKernelFull",
            dependencies: ["FantasticUniFFIFull"],
            path: "Sources/FantasticKernelFull"
        ),
        // The binary XCFramework that ships the full-tier Rust kernel.
        .binaryTarget(
            name: "FantasticUniFFIFull",
            path: "Fantastic-Full.xcframework"
        ),
    ]
)
