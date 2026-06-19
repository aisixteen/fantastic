// Recursive substrate node.
//
// Mirrors Rust's `fantastic_kernel::Agent`. One node per agent in the
// kernel tree; routing happens through `Kernel.agents` map (flat),
// not the parent/child chain — the children map is just for cascade
// delete + persistence walk.
//
// Locking model mirrors Rust: meta and watcherIds are mutated under
// separate locks, ephemeral immutable fields are public. The class
// is `@unchecked Sendable` because we manually verify lock discipline
// at every mutation site.

import FantasticJSON
import Foundation
import OrderedCollections

public final class Agent: @unchecked Sendable {
    /// Stable identifier (unique across the tree).
    public let id: AgentId
    /// Bundle handler key, or `nil` for the root + other bare agents.
    public let handlerModule: String?
    /// Parent agent's id, or `nil` for the root.
    public let parentId: AgentId?
    /// Filesystem path where this agent's `agent.json` lives.
    /// Computed regardless of StorageMode — Disk uses it, InMemory
    /// ignores it.
    public let rootPath: URL
    /// True for agents that should never be persisted (CLI fixtures,
    /// REPL-spawned). Kernel's `register` honors this.
    public let ephemeral: Bool

    private let metaLock = NSLock()
    private var _meta: OrderedDictionary<String, JSON>

    private let watcherLock = NSLock()
    private var _watcherIds: Set<AgentId> = []

    private let childrenLock = NSLock()
    private var _children: OrderedDictionary<AgentId, Agent> = [:]

    public init(
        id: AgentId,
        handlerModule: String?,
        parentId: AgentId?,
        meta: OrderedDictionary<String, JSON> = [:],
        rootPath: URL = URL(fileURLWithPath: ""),
        ephemeral: Bool = false
    ) {
        self.id = id
        self.handlerModule = handlerModule
        self.parentId = parentId
        self._meta = meta
        self.rootPath = rootPath
        self.ephemeral = ephemeral
    }

    // ── meta access ─────────────────────────────────────────────

    public var meta: OrderedDictionary<String, JSON> {
        metaLock.lock()
        defer { metaLock.unlock() }
        return _meta
    }

    public func metaValue(forKey key: String) -> JSON? {
        metaLock.lock()
        defer { metaLock.unlock() }
        return _meta[key]
    }

    public var displayName: String? {
        metaValue(forKey: "display_name")?.asString
    }

    /// Optional short `description` (from meta) — a one-line "what this
    /// agent does", surfaced in every reflect. Set via
    /// create_agent / update_agent. (Named `descriptionMeta` to avoid
    /// colliding with `CustomStringConvertible.description`.)
    public var descriptionMeta: String? {
        metaValue(forKey: "description")?.asString
    }

    public var isDeleteLocked: Bool {
        metaValue(forKey: "delete_lock")?.asBool ?? false
    }

    /// Merge keys from `patch` into `meta`. Returns the updated
    /// record snapshot.
    @discardableResult
    public func updateMeta(_ patch: OrderedDictionary<String, JSON>) -> AgentRecord {
        metaLock.lock()
        for (k, v) in patch {
            _meta[k] = v
        }
        metaLock.unlock()
        return record()
    }

    // ── record / paths ──────────────────────────────────────────

    public func record() -> AgentRecord {
        AgentRecord(
            id: id.value,
            handlerModule: handlerModule,
            parentId: parentId?.value,
            meta: meta
        )
    }

    public var agentFile: URL {
        rootPath.appendingPathComponent("agent.json")
    }

    public var childrenDir: URL {
        rootPath.appendingPathComponent("agents")
    }

    public var readmeFile: URL {
        rootPath.appendingPathComponent("readme.md")
    }

    // ── children (cascade-delete + persist walk) ────────────────

    func insertChild(_ child: Agent) {
        childrenLock.lock()
        defer { childrenLock.unlock() }
        _children[child.id] = child
    }

    @discardableResult
    func removeChild(_ id: AgentId) -> Agent? {
        childrenLock.lock()
        defer { childrenLock.unlock() }
        return _children.removeValue(forKey: id)
    }

    public func hasChild(_ id: AgentId) -> Bool {
        childrenLock.lock()
        defer { childrenLock.unlock() }
        return _children[id] != nil
    }

    public var childCount: Int {
        childrenLock.lock()
        defer { childrenLock.unlock() }
        return _children.count
    }

    /// Direct child ids, sorted alphabetically for deterministic
    /// iteration.
    public func childIds() -> [AgentId] {
        childrenLock.lock()
        let ids = Array(_children.keys)
        childrenLock.unlock()
        return ids.sorted { $0.value < $1.value }
    }

    public func children() -> [Agent] {
        childrenLock.lock()
        defer { childrenLock.unlock() }
        return Array(_children.values)
    }

    // ── watcher ids ─────────────────────────────────────────────

    public func addWatcher(_ id: AgentId) {
        watcherLock.lock()
        defer { watcherLock.unlock() }
        _watcherIds.insert(id)
    }

    public func removeWatcher(_ id: AgentId) {
        watcherLock.lock()
        defer { watcherLock.unlock() }
        _watcherIds.remove(id)
    }

    public func watcherIds() -> Set<AgentId> {
        watcherLock.lock()
        defer { watcherLock.unlock() }
        return _watcherIds
    }
}
