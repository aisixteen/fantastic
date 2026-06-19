// Storage backing for apple_kvs — a thin string-keyed/string-valued store.
//
// Values are opaque serialized-JSON strings (the consumer defines the shape).
// Two backings:
//   - UbiquitousKVSBacking — NSUbiquitousKeyValueStore.default (Apple, synced).
//   - MemoryKVSBacking      — an in-process dict (tests + non-Apple stub).
// The bundle talks only to this seam, so data-op tests are deterministic
// regardless of iCloud entitlement / sign-in.

import Foundation

/// One synced KV namespace's raw byte store. All values are strings
/// (serialized JSON); apple_kvs is value-agnostic.
public protocol KVSBacking: Sendable {
    func get(_ key: String) -> String?
    func set(_ value: String, forKey key: String)
    func remove(_ key: String)
    /// Every key→value we manage (string values only).
    func all() -> [String: String]
    /// Upload hint (not a flush). Returns whether the store accepted it.
    @discardableResult func synchronize() -> Bool
}

/// In-memory backing — used by tests and as the non-Apple fallback. Never
/// synced; the bundle gates all access behind availability anyway.
public final class MemoryKVSBacking: KVSBacking, @unchecked Sendable {
    private let lock = NSLock()
    private var store: [String: String] = [:]

    public init() {}

    public func get(_ key: String) -> String? {
        lock.lock(); defer { lock.unlock() }
        return store[key]
    }
    public func set(_ value: String, forKey key: String) {
        lock.lock(); store[key] = value; lock.unlock()
    }
    public func remove(_ key: String) {
        lock.lock(); store.removeValue(forKey: key); lock.unlock()
    }
    public func all() -> [String: String] {
        lock.lock(); defer { lock.unlock() }
        return store
    }
    public func synchronize() -> Bool { true }
}

#if canImport(Darwin)
    /// iCloud KVS backing. Sync requires the
    /// `com.apple.developer.ubiquity-kvstore-identifier` entitlement (an app
    /// provisioning step); without it this still works as a local cache, but
    /// the bundle's availability gate keeps us from trusting it offline.
    final class UbiquitousKVSBacking: KVSBacking, @unchecked Sendable {
        private let kvs = NSUbiquitousKeyValueStore.default

        func get(_ key: String) -> String? { kvs.string(forKey: key) }
        func set(_ value: String, forKey key: String) { kvs.set(value, forKey: key) }
        func remove(_ key: String) { kvs.removeObject(forKey: key) }
        func all() -> [String: String] {
            var out: [String: String] = [:]
            for (k, v) in kvs.dictionaryRepresentation {
                if let s = v as? String { out[k] = s }
            }
            return out
        }
        @discardableResult func synchronize() -> Bool { kvs.synchronize() }
    }
#endif

/// The backing the registered bundle uses: iCloud KVS on Apple, in-memory
/// elsewhere (the elsewhere path is never reachable in practice — the agent
/// reports unavailable on non-Apple — but keeps the target cross-compilable).
public func defaultKVSBacking() -> KVSBacking {
    #if canImport(Darwin)
        return UbiquitousKVSBacking()
    #else
        return MemoryKVSBacking()
    #endif
}
