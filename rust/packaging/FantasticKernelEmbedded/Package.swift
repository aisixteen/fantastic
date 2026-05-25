// swift-tools-version:5.9
//
// FantasticKernelEmbedded — sandboxed tier of the Rust kernel as a Swift
// Package. Wraps `Fantastic-Embedded.xcframework`.
//
// Use this from FantasticLite (multi-platform: iOS, iPadOS, macOS Lite).
// The embedded tier compile-time excludes PTY/subprocess bundles
// (terminal_backend, local_runner, python_runtime, ssh_runner) so the
// resulting binary is iOS-sandbox-safe.
//
// Pro Mac uses the sister package `FantasticKernelFull` which has the
// full bundle set with PTY support.
//
// Consume from project.yml:
//   packages:
//     FantasticKernelEmbedded:
//       path: ../../fantastic_canvas/rust/packaging/FantasticKernelEmbedded
//   targets:
//     FantasticLite:
//       dependencies:
//         - package: FantasticKernelEmbedded
//           product: FantasticKernelEmbedded
//
// The XCFramework is built by `rust/scripts/build-xcframework-embedded.sh`.

import PackageDescription

let package = Package(
    name: "FantasticKernelEmbedded",
    platforms: [
        .macOS(.v13),
        .iOS(.v16),
        .tvOS(.v16),
        .visionOS(.v1),
        .watchOS(.v9),
    ],
    products: [
        .library(
            name: "FantasticKernelEmbedded",
            targets: ["FantasticKernelEmbedded"]
        ),
    ],
    targets: [
        // The Swift wrapper layer (re-exports the UniFFI-generated bindings).
        .target(
            name: "FantasticKernelEmbedded",
            dependencies: ["FantasticUniFFIEmbedded"],
            path: "Sources/FantasticKernelEmbedded"
        ),
        // The binary XCFramework that ships the embedded-tier Rust kernel.
        // Apple's Xcode 16+ SPM validation requires the .xcframework
        // directory basename to match the binary-target name exactly.
        .binaryTarget(
            name: "FantasticUniFFIEmbedded",
            path: "FantasticUniFFIEmbedded.xcframework"
        ),
    ]
)
