// Registry of bundles available to a kernel instance.
//
// Mirrors Rust's `fantastic_kernel::BundleRegistry`. Compile-time
// composition: the CLI / app constructs the registry with the bundle
// set it wants, then hands it to `Kernel.init`.

import Foundation

public final class BundleRegistry: @unchecked Sendable {
    private let lock = NSLock()
    private var map: [String: any AgentBundle] = [:]
    private var unlisted: Set<String> = []

    public init() {}

    /// Install an INSTALLABLE `bundle` under `handlerModule` — it dispatches
    /// AND appears in the `reflect bundles=…` catalog. Replaces any prior
    /// entry for the same key.
    public func register(_ handlerModule: String, _ bundle: any AgentBundle) {
        lock.lock()
        defer { lock.unlock() }
        unlisted.remove(handlerModule)
        map[handlerModule] = bundle
    }

    /// Install a bundle that DISPATCHES but is not installable — python's
    /// importable module with no `fantastic.bundles` entry point (the e2e
    /// scenario probes). It never appears in the `bundles` catalog.
    public func registerUnlisted(_ handlerModule: String, _ bundle: any AgentBundle) {
        lock.lock()
        defer { lock.unlock() }
        unlisted.insert(handlerModule)
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

    /// The INSTALLED (handler_module, bundle) pairs — python's entry-point
    /// scan. The `reflect bundles=all|ids` catalog; unlisted bundles are
    /// skipped.
    public func catalog() -> [(String, any AgentBundle)] {
        lock.lock()
        defer { lock.unlock() }
        return map.filter { !unlisted.contains($0.key) }.map { ($0.key, $0.value) }
    }
}
