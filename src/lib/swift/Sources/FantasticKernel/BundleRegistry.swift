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
    /// FOLDER BUNDLES of this kernel's root: name → the bundle
    /// its base built. Loaded at boot and re-loaded by `reload_bundle`.
    private var folders: [String: any AgentBundle] = [:]
    /// What `reflect bundles=all` lists for each folder — a refused one too, with
    /// its `error`.
    private var folderRowMap: [String: JSON] = [:]

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
    /// `register_unlisted`: a module with no `fantastic.bundles` entry point
    /// (the e2e scenario probes). It never appears in the `bundles` catalog.
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
        return map[handlerModule] ?? folders[handlerModule]
    }

    /// Load a folder bundle: `name` resolves to `bundle` (a reload replaces it).
    public func registerFolder(_ name: String, _ bundle: any AgentBundle, row: JSON) {
        lock.lock()
        defer { lock.unlock() }
        folders[name] = bundle
        folderRowMap[name] = row
    }

    /// A folder that could not load: listed (with its `error`), never resolvable —
    /// unless an earlier load stands.
    public func refuseFolder(_ name: String, row: JSON) {
        lock.lock()
        defer { lock.unlock() }
        // a failed RELOAD keeps an earlier load (module and row): its live
        // instances keep running it; the error goes to the reload's caller
        if folders[name] != nil { return }
        folderRowMap[name] = row
    }

    /// Every folder bundle this kernel read — loaded and refused — by name.
    public func folderRows() -> [JSON] {
        lock.lock()
        defer { lock.unlock() }
        return folderRowMap.keys.sorted().compactMap { folderRowMap[$0] }
    }

    /// Is `handlerModule` a loaded folder bundle?
    public func isFolder(_ handlerModule: String) -> Bool {
        lock.lock()
        defer { lock.unlock() }
        return folders[handlerModule] != nil
    }

    /// Is `name` a registered bundle's handler module, its package, or its name?
    /// A folder may not shadow one.
    public func clashes(_ name: String) -> Bool {
        lock.lock()
        defer { lock.unlock() }
        return map.contains { hm, b in
            hm == name || hm.split(separator: ".").first.map(String.init) == name || b.name == name
        }
    }

    /// The installed bundles a folder can extend, by name, sorted.
    public func folderBases() -> [String] {
        catalog().filter { $0.1.folderBase }.map { $0.1.name }.sorted()
    }

    /// The installed bundle named `name`, if any.
    public func byName(_ name: String) -> (any AgentBundle)? {
        catalog().first { $0.1.name == name }?.1
    }

    /// Python's `import_handler(handlerModule)` against this registry: `nil`
    /// when a bundle is registered under `handlerModule`, else the
    /// `ModuleNotFoundError` text. Python resolves a handler_module ONLY from its
    /// bundle catalog plus `register_unlisted`, and
    /// names the TOP-LEVEL package of any other name — `absent_bundle.tools`,
    /// `file_bridge.nope` and the bare package `file_bridge` alike → `No module
    /// named '<top>'` (rust's `no_module_named`).
    public func moduleNotFound(_ handlerModule: String) -> String? {
        if get(handlerModule) != nil { return nil }
        let top = handlerModule.split(separator: ".", omittingEmptySubsequences: false)
            .first.map(String.init) ?? handlerModule
        return "No module named \(pyRepr(top))"
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
