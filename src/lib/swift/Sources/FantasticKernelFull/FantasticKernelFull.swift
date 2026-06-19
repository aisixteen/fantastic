// FantasticKernelFull — desktop / unsandboxed-tier umbrella module.
//
// Same public re-export set as `FantasticKernelEmbedded`. The
// difference is intentionally not in this module's source — both
// products import the same kernel package, and the Pro-tier
// subprocess bundles (`terminal_backend`, `local_runner`,
// `python_runtime`, `ssh_runner`) are linked in automatically via
// the kernel's `#if os(macOS)` paths when the consuming app target
// is macOS-only.
//
//     import FantasticKernelFull
//
//     let kernel = try await startKernel(
//         workdir: workspaceURL.path,
//         portHint: 0
//     )
//
// FantasticPro consumes this product from a macOS-only app target
// (Developer ID, unsandboxed). Importing on iOS is technically
// allowed but the subprocess bundles fall away — equivalent to
// importing `FantasticKernelEmbedded` in that case.

@_exported import FantasticJSON
@_exported import FantasticKernel
@_exported import FantasticKernelStartup
@_exported import FantasticProxyAgent
@_exported import FantasticTools

import Foundation

/// Identity stub for the Pro-tier umbrella. Mirrors
/// `FantasticKernelEmbeddedInfo` so app code can branch on tier at
/// runtime without conditional imports.
public enum FantasticKernelFullInfo {
    public static let version = "0.2.0-swift"
    public static let isSwiftNative = true
}
