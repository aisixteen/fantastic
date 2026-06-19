// apple_kvs — a synced, cross-device key-value store agent backed by Apple
// iCloud KVS (NSUbiquitousKeyValueStore). The reusable storage primitive for
// small, per-entry, cross-device collections (the kernel/runner directory is
// the first consumer). Sibling-in-spirit to yaml_state, but synced + LIVE-ONLY:
// no local persistence, no fallback — unavailable when signed-out or offline.
//
// A single agent IS the whole default KVS; callers namespace keys as
// "<collection>.<id>" (e.g. nodes.<uuid>) and `list` filters by prefix.
//
// This is the canvas-side deliverable. Consumers (the nodes directory, recents
// migration, app wiring) and the ubiquity-kvstore entitlement are app work.

import FantasticJSON
import FantasticKernel
import Foundation
import OrderedCollections

public struct AppleKVSBundle: AgentBundle {
    public let name = "apple_kvs"

    let store: KVSBacking
    let manager: AppleKVSManager

    public init(
        store: KVSBacking = defaultKVSBacking(),
        manager: AppleKVSManager = AppleKVSManager()
    ) {
        self.store = store
        self.manager = manager
    }

    public var readme: String? { Self.readmeText }

    // KVS hard limits (surfaced in reflect, enforced on set).
    static let maxTotalBytes = 1_048_576
    static let maxKeys = 1024
    static let maxValueBytes = 1_048_576

    public func handle(agentId: AgentId, payload: JSON, kernel: Kernel) async throws -> JSON? {
        let verb = payload["type"].asString ?? ""
        switch verb {
        case "reflect":
            return reflectReply(agentId)
        case "boot":
            manager.startObservers(agentId: agentId, kernel: kernel)
            return .null
        case "shutdown":
            manager.stopObservers(agentId: agentId)
            return .null
        case "watch":
            // Ensure the change observers are live; the actual subscription is
            // the kernel's standard watch (watchers get our emitted events).
            manager.startObservers(agentId: agentId, kernel: kernel)
            let (ok, reason) = manager.available()
            var o: OrderedDictionary<String, JSON> = [
                "watching": .bool(true), "synced": .bool(ok),
            ]
            if let reason { o["reason"] = .string(reason) }
            return .object(o)
        case "set", "read", "delete", "list":
            let (ok, reason) = manager.available()
            guard ok else {
                return .object([
                    "unavailable": .bool(true),
                    "reason": .string(reason ?? "unavailable"),
                ])
            }
            return dataVerb(verb, payload)
        default:
            return .object(["error": .string("apple_kvs: unknown type \"\(verb)\"")])
        }
    }

    public func onDelete(agentId: AgentId, kernel: Kernel) async throws {
        manager.stopObservers(agentId: agentId)
    }

    public func onShutdown(agentId: AgentId, kernel: Kernel) async throws {
        manager.stopObservers(agentId: agentId)
    }

    // ── data verbs (only reached when available) ────────────────

    private func dataVerb(_ verb: String, _ payload: JSON) -> JSON {
        switch verb {
        case "set":
            guard let key = payload["key"].asString, !key.isEmpty else {
                return .object(["error": .string("apple_kvs.set: key (non-empty str) required")])
            }
            guard let value = payload.asObject?["value"] else {
                return .object(["error": .string("apple_kvs.set: value required")])
            }
            let serialized = value.serialize()
            let vbytes = serialized.utf8.count
            if vbytes > Self.maxValueBytes {
                return overLimit("value is \(vbytes)B (> \(Self.maxValueBytes)B per-value limit)")
            }
            let all = store.all()
            let prior = all[key]
            if prior == nil && all.count >= Self.maxKeys {
                return overLimit("at \(Self.maxKeys)-key limit")
            }
            let curTotal = all.values.reduce(0) { $0 + $1.utf8.count }
            let newTotal = curTotal - (prior?.utf8.count ?? 0) + vbytes
            if newTotal > Self.maxTotalBytes {
                return overLimit("would exceed \(Self.maxTotalBytes)B total")
            }
            store.set(serialized, forKey: key)
            store.synchronize()
            return .object(["key": .string(key), "set": .bool(true)])

        case "read":
            guard let key = payload["key"].asString else {
                return .object(["error": .string("apple_kvs.read: key required")])
            }
            let value = store.get(key).flatMap { try? JSON.parse($0) } ?? .null
            return .object(["key": .string(key), "value": value])

        case "delete":
            guard let key = payload["key"].asString else {
                return .object(["error": .string("apple_kvs.delete: key required")])
            }
            let existed = store.get(key) != nil
            store.remove(key)
            store.synchronize()
            return .object(["key": .string(key), "deleted": .bool(existed)])

        case "list":
            let prefix = payload["prefix"].asString
            let all = store.all()
            let items: [JSON] = all.keys
                .filter { prefix == nil || $0.hasPrefix(prefix!) }
                .sorted()
                .map { k in
                    .object(["key": .string(k), "size": .integer(Int64(all[k]!.utf8.count))])
                }
            return .object(["keys": .array(items)])

        default:
            return .object(["error": .string("apple_kvs: unknown type \"\(verb)\"")])
        }
    }

    private func overLimit(_ why: String) -> JSON {
        .object([
            "error": .string("apple_kvs.set: \(why)"),
            "hint": .string("migrate to CloudKit for larger / unbounded storage"),
        ])
    }

    // ── reflect ─────────────────────────────────────────────────

    private func reflectReply(_ agentId: AgentId) -> JSON {
        let (ok, reason) = manager.available()
        var out: OrderedDictionary<String, JSON> = [
            "id": .string(agentId.value),
            "sentence": .string(Self.sentence),
            "backing": .string("apple_kvs"),
            "available": .bool(ok),
            "synced": .bool(ok),
        ]
        if let reason { out["reason"] = .string(reason) }

        // LIVE-ONLY: when unavailable we do NOT read/trust the local cache.
        if ok {
            let all = store.all()
            let bytes = all.values.reduce(0) { $0 + $1.utf8.count }
            let namespaces = Set(
                all.keys.map { k -> String in
                    guard let dot = k.firstIndex(of: ".") else { return k }
                    return String(k[..<dot])
                }
            ).sorted()
            out["key_count"] = .integer(Int64(all.count))
            out["namespaces"] = .array(namespaces.map { .string($0) })
            out["usage"] = .object(["bytes": .integer(Int64(bytes)), "keys": .integer(Int64(all.count))])
        } else {
            out["key_count"] = .integer(0)
            out["namespaces"] = .array([])
            out["usage"] = .object(["bytes": .integer(0), "keys": .integer(0)])
        }
        out["limits"] = .object([
            "bytes": .integer(Int64(Self.maxTotalBytes)),
            "keys": .integer(Int64(Self.maxKeys)),
            "value_bytes": .integer(Int64(Self.maxValueBytes)),
        ])
        out["verbs"] = .object([
            "set": .string("args: key:str, value:any. Upsert one record; schedules sync."),
            "read": .string("args: key:str. Value at key (null if absent). (`get` is a kernel system verb, so the read verb is `read`.)"),
            "delete": .string("args: key:str. Remove a record."),
            "list": .string("args: prefix?:str. Keys (+sizes), optionally by collection prefix."),
            "watch": .string("args: prefix?:str. Ensure cross-device observers; emits {type:changed,keys} / {type:availability} on this inbox."),
        ])
        return .object(out)
    }

    static let sentence =
        "A synced, cross-device key-value store (iCloud KVS). LIVE-ONLY: writes "
        + "propagate to your other devices, but the store is unavailable when "
        + "signed out of iCloud or offline — there is no local copy. Namespace "
        + "keys as `<collection>.<id>`; `watch` to see changes from other devices."

    static let readmeText = """
        # apple_kvs — synced key-value store agent (Apple iCloud KVS)

        A small, cross-device key-value store as a Fantastic agent, backed by
        `NSUbiquitousKeyValueStore`. The reusable primitive for per-entry synced
        collections (the kernel/runner directory `nodes` is the first consumer;
        recents/prefs/hosts/pins follow). One agent IS the whole default KVS —
        **callers namespace keys** as `"<collection>.<id>"` and `list` filters by
        prefix. Values are opaque (serialized JSON the consumer defines).

        ## Availability — LIVE-ONLY (no local persistence, no fallback)
        Available **iff** signed into iCloud (`ubiquityIdentityToken != nil`) **and**
        the network is up. Otherwise every data verb returns
        `{unavailable:true, reason}` — we never read or trust the OS's on-disk KVS
        cache. Identity change swaps to the new account or empties. The directory
        *is* the cross-device feature; offline you simply don't get it.

        ## The `kv` surface (verbs)
        - `set {key, value}` — upsert one record (+ `synchronize()` upload hint).
        - `read {key}` — value at key (null if absent). NOTE: the read verb is
          `read`, not `get` — `get` is a reserved kernel system verb (agent record).
        - `delete {key}` — remove a record.
        - `list {prefix?}` — keys + sizes, optionally by collection prefix.
        - `watch {prefix?}` — ensure cross-device observers; this agent then emits
          `{type:"changed", keys:[...]}` on each external (other-device) write and
          `{type:"availability", available, reason}` on sign-in/network change.
          Subscribe with the kernel's standard watch on this agent's id; `prefix`
          is advisory (watchers filter the emitted keys).
        - `reflect` — `{backing, available, synced, reason?, key_count, namespaces,
          usage, limits, verbs}`.

        This is the named, reusable `kv` contract — a later impl (`cloudkit_kv`,
        another platform's synced store) can be a drop-in by answering these verbs.

        ## Limits (enforced on set, surfaced in reflect)
        1 MB total · 1024 keys · 1 MB per value. Oversize `set` →
        `{error, hint:"migrate to CloudKit ..."}`.

        ## Note
        Actual cross-device propagation requires the
        `com.apple.developer.ubiquity-kvstore-identifier` entitlement (an app
        provisioning step). The bare kernel binary has no entitlement, so writes
        won't propagate there even when `available:true`.
        """
}
