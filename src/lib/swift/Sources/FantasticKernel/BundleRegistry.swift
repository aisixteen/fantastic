// Registry of bundles available to a kernel instance.
//
// Mirrors Rust's `fantastic_kernel::BundleRegistry`. Compile-time
// composition: the CLI / app constructs the registry with the bundle
// set it wants, then hands it to `Kernel.init`.

import Foundation

public final class BundleRegistry: @unchecked Sendable {
    private let lock = NSLock()
    private var map: [String: any AgentBundle] = [:]

    public init() {}

    /// Install `bundle` under `handlerModule`. Replaces any prior
    /// entry for the same key.
    public func register(_ handlerModule: String, _ bundle: any AgentBundle) {
        lock.lock()
        defer { lock.unlock() }
        map[handlerModule] = bundle
    }

    /// Look up the bundle for `handlerModule`, or `nil` if none.
    public func get(_ handlerModule: String) -> (any AgentBundle)? {
        lock.lock()
        defer { lock.unlock() }
        return map[handlerModule]
    }

    /// All registered handler_module → bundle pairs (snapshot).
    public func snapshot() -> [(String, any AgentBundle)] {
        lock.lock()
        defer { lock.unlock() }
        return map.map { ($0.key, $0.value) }
    }
}
