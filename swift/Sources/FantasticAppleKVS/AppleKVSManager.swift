// Availability gate + cross-device change observers for apple_kvs.
//
// Availability model (DECIDED, see the bundle readme): the store is
// LIVE-ONLY — available iff signed into iCloud (ubiquityIdentityToken != nil)
// AND the network is up (NWPathMonitor .satisfied). Either false → the bundle
// gates every data verb; we never read/trust the OS's on-disk KVS cache.
//
// Observers (registered on boot/watch, torn down on delete/shutdown) push
// change events onto the agent's own inbox via kernel.emit, which the kernel
// fans out to watchers:
//   - didChangeExternally → {type:"changed", keys:[...]}   (cross-device write)
//   - identity change      → {type:"availability", identity_changed:true, ...}
//   - network change       → {type:"availability", ...}

import FantasticJSON
import FantasticKernel
import Foundation

#if canImport(Network)
    import Network
#endif

/// Owns availability probing + the long-lived OS observers. One instance is
/// shared by the single registered bundle (a `let` reference, so the bundle
/// struct stays Sendable). Lock-protected.
public final class AppleKVSManager: @unchecked Sendable {
    /// Test seam: when set, bypasses the real iCloud/network probe.
    public typealias AvailabilityCheck = @Sendable () -> (Bool, String?)

    private let lock = NSLock()
    private let availabilityOverride: AvailabilityCheck?
    private weak var kernelRef: Kernel?
    private var observed: [AgentId: [NSObjectProtocol]] = [:]

    // Network state, tracked by a lazily-started monitor (no override only).
    private var networkUp = true
    private var monitorStarted = false
    #if canImport(Network)
        private var monitor: NWPathMonitor?
        private let monitorQueue = DispatchQueue(label: "apple_kvs.netmon")
    #endif

    public init(availabilityOverride: AvailabilityCheck? = nil) {
        self.availabilityOverride = availabilityOverride
    }

    // ── availability ────────────────────────────────────────────

    /// `(available, reason?)`. Reason is non-nil only when unavailable.
    public func available() -> (Bool, String?) {
        if let probe = availabilityOverride { return probe() }
        #if canImport(Darwin)
            ensureMonitor()
            guard FileManager.default.ubiquityIdentityToken != nil else {
                return (false, "not signed into iCloud")
            }
            lock.lock()
            let net = networkUp
            lock.unlock()
            return net ? (true, nil) : (false, "network unavailable")
        #else
            return (false, "apple_kvs needs an Apple platform")
        #endif
    }

    private func ensureMonitor() {
        // With a test availability override, skip the live monitor entirely so
        // its initial/async path callbacks don't emit availability events.
        if availabilityOverride != nil { return }
        #if canImport(Network)
            lock.lock()
            if monitorStarted {
                lock.unlock()
                return
            }
            monitorStarted = true
            let m = NWPathMonitor()
            monitor = m
            lock.unlock()
            m.pathUpdateHandler = { [weak self] path in
                self?.onNetworkChange(path.status == .satisfied)
            }
            m.start(queue: monitorQueue)
        #endif
    }

    private func onNetworkChange(_ up: Bool) {
        lock.lock()
        let changed = networkUp != up
        networkUp = up
        let agents = Array(observed.keys)
        lock.unlock()
        guard changed else { return }
        for id in agents { emitAvailability(id, identityChanged: false) }
    }

    // ── observers ───────────────────────────────────────────────

    /// Idempotent: register the OS observers for `agentId` (no-op if already
    /// observing). Stores the kernel handle for later emits.
    public func startObservers(agentId: AgentId, kernel: Kernel) {
        lock.lock()
        kernelRef = kernel
        if observed[agentId] != nil {
            lock.unlock()
            return
        }
        observed[agentId] = []
        lock.unlock()
        ensureMonitor()
        #if canImport(Darwin)
            let nc = NotificationCenter.default
            let changedTok = nc.addObserver(
                forName: NSUbiquitousKeyValueStore.didChangeExternallyNotification,
                object: nil, queue: nil
            ) { [weak self] note in
                self?.emitChanged(agentId, note)
            }
            let identityTok = nc.addObserver(
                forName: .NSUbiquityIdentityDidChange,
                object: nil, queue: nil
            ) { [weak self] _ in
                self?.emitAvailability(agentId, identityChanged: true)
            }
            lock.lock()
            observed[agentId] = [changedTok, identityTok]
            lock.unlock()
        #endif
    }

    /// Remove `agentId`'s observers (and stop the monitor if it's the last).
    public func stopObservers(agentId: AgentId) {
        lock.lock()
        let tokens = observed.removeValue(forKey: agentId) ?? []
        let empty = observed.isEmpty
        lock.unlock()
        #if canImport(Darwin)
            for t in tokens { NotificationCenter.default.removeObserver(t) }
        #endif
        if empty { stopMonitor() }
    }

    private func stopMonitor() {
        #if canImport(Network)
            lock.lock()
            let m = monitor
            monitor = nil
            monitorStarted = false
            lock.unlock()
            m?.cancel()
        #endif
    }

    // ── emit helpers (marshal OS callbacks onto the kernel) ──────

    private func emitChanged(_ agentId: AgentId, _ note: Notification) {
        var keys: [String] = []
        #if canImport(Darwin)
            if let raw = note.userInfo?[NSUbiquitousKeyValueStoreChangedKeysKey] as? [String] {
                keys = raw
            }
        #endif
        emit(agentId, .object([
            "type": .string("changed"),
            "keys": .array(keys.map { .string($0) }),
        ]))
    }

    private func emitAvailability(_ agentId: AgentId, identityChanged: Bool) {
        let (ok, reason) = available()
        var o: [String: JSON] = [
            "type": .string("availability"),
            "available": .bool(ok),
            "synced": .bool(ok),
        ]
        if let r = reason { o["reason"] = .string(r) }
        if identityChanged { o["identity_changed"] = .bool(true) }
        emit(agentId, .object(.init(uniqueKeysWithValues: o.map { ($0, $1) })))
    }

    private func emit(_ agentId: AgentId, _ payload: JSON) {
        lock.lock()
        let kernel = kernelRef
        lock.unlock()
        guard let kernel else { return }
        Task { await kernel.emit(agentId, payload) }
    }
}
