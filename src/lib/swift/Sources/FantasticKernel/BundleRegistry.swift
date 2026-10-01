// Registry of bundles available to a kernel instance.
//
// Mirrors Rust's `fantastic_kernel::BundleRegistry`. Compile-time
// composition: the CLI / app constructs the registry with the bundle
// set it wants, then hands it to `Kernel.init`.

import FantasticJSON
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

    /// Python `importlib.import_module(handlerModule)` against this registry:
    /// `nil` when the import resolves (a registered module, or a package —
    /// a dotted prefix of one), else the `ModuleNotFoundError` text. The
    /// import walks the dotted prefixes and names the FIRST one that does not
    /// resolve (`absent_bundle.tools` → `No module named 'absent_bundle'`);
    /// a child of a plain module adds `; '<parent>' is not a package`.
    public func moduleNotFound(_ handlerModule: String) -> String? {
        lock.lock()
        let keys = Array(map.keys)
        lock.unlock()
        func isPackage(_ name: String) -> Bool {
            keys.contains { $0.hasPrefix(name + ".") }
        }
        let parts = handlerModule.split(separator: ".", omittingEmptySubsequences: false)
        var parent: String?
        for i in 1...parts.count {
            let name = parts[0..<i].joined(separator: ".")
            if let p = parent, !isPackage(p) {
                return "No module named \(pyRepr(name)); \(pyRepr(p)) is not a package"
            }
            if !keys.contains(name) && !isPackage(name) {
                return "No module named \(pyRepr(name))"
            }
            parent = name
        }
        return nil
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
