// FantasticKernelEmbedded — sandboxed-tier umbrella module.
//
// Re-exports the public surface of the native Swift kernel under a
// stable product name so the Apple app's
//
//     import FantasticKernelEmbedded
//
//     let kernel = try await startKernelInMemory(portHint: 0)
//     let port = kernel.httpPort()
//     // ... and `await kernel.shutdown()` when the app stops
//
// The Lite tier targets iOS, iPadOS, visionOS, tvOS, watchOS, and
// sandboxed macOS. Sandbox-incompatible bundles (terminal_backend,
// exec_bridge) are compile-time
// excluded by `#if os(macOS)` gates inside the kernel — importing
// this module on iOS is safe and never reaches subprocess code.

@_exported import FantasticJSON
@_exported import FantasticKernel
@_exported import FantasticKernelStartup

import Foundation

/// Identity stub. Lets consumers verify they're linked against the
/// native Swift kernel (pre-0.2.0 versions wrapped a Rust
/// XCFramework that's no longer in the picture).
public enum FantasticKernelEmbeddedInfo {
    public static let version = "0.2.0-swift"
    public static let isSwiftNative = true
}
