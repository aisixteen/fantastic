// FantasticKernel — Swift wrapper over the UniFFI-generated bindings.
//
// The auto-generated `fantastic.swift` (produced by
// `scripts/build-xcframework.sh`) sits alongside this file and is
// re-exported. We keep this file present so the Sources directory
// isn't empty before the build script runs.
//
// Consumers should import `FantasticKernel` and use the API directly:
//
//     import FantasticKernel
//
//     let kernel = try await Fantastic.startKernel(
//         workdir: appGroupURL.path,
//         portHint: 0
//     )
//     let port = kernel.httpPort()
//     // ... point WKWebView at http://127.0.0.1:\(port)/<canvas_id>/
//     defer { kernel.shutdown() }

import Foundation

/// Convenience namespace + version constant. The generated UniFFI
/// bindings (fantastic.swift) expose the actual API.
public enum FantasticKernel {
    /// Library version. Bumped alongside Cargo.toml's
    /// `[workspace.package].version`.
    public static let version = "0.1.0"
}
