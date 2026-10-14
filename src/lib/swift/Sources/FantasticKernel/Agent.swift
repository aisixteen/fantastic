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
    /// Filesystem path where this agent's `agent.yaml` lives.
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

    private let inFlightLock = NSLock()
    private var _inFlight: Int = 0

    /// Set for an agent on a CONNECTED PEER KERNEL (python's `RemoteTarget`):
    /// built by one send for `<guid>|<agent id>`, never registered in this tree,
    /// and dispatched through the peer's route instead of a handler here. Its
    /// `parentId` is `nil` like python's `parent = None`, so every path that reads
    /// "no parent" as "the root" must ask `isRemote` first.
    let remote: RemoteRef?

    /// Is this an agent on a connected peer kernel (see `init(remote:_:)`)?
    public var isRemote: Bool { remote != nil }

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
        // The authority baseline, stamped + MERGED on BOTH fields for EVERY
        // agent that passes through here — the seeded root, a created child, a
        // disk-loaded record alike (one funnel, so nothing lacks it). `can_call`
        // (who I may call) and `callable_by` (who may call me) both start as
        // {$self:*} — nothing else. It MERGES UNDER any explicit grant
        // already in `meta` (idempotent on reload). Deny-all governs every edge NOT
        // in the merged grant.
        var stamped = meta
        for (field, base) in [
            ("can_call", defaultCanCall()), ("callable_by", defaultCallableBy()),
        ] {
            guard case .object(var merged) = base else { continue }
            if case .object(let explicit) = stamped[field] ?? .null {
                for (k, v) in explicit { merged[k] = v }
            }
            stamped[field] = .object(merged)
        }
        self._meta = stamped
        self.rootPath = rootPath
        self.ephemeral = ephemeral
        self.remote = nil
    }

    /// The target ONE send resolved `<guid>|<agent id>` to on a connected peer.
    /// No baseline: it is nobody's parent or child here, and its
    /// `callable_by` is python's empty map — whether it accepts a call is decided
    /// on its own kernel.
    init(remote qualified: AgentId, _ ref: RemoteRef) {
        self.id = qualified
        self.handlerModule = nil
        self.parentId = nil
        self._meta = ["callable_by": .object([:])]
        self.rootPath = URL(fileURLWithPath: "")
        self.ephemeral = true
        self.remote = ref
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

    /// python `if self._meta.get("delete_lock")` — TRUTHINESS: `1`, `"yes"`,
    /// `"false"` (a non-empty string) and `[x]` all lock, not only JSON `true`.
    public var isDeleteLocked: Bool {
        pyTruthy(metaValue(forKey: "delete_lock") ?? .null)
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
        rootPath.appendingPathComponent(RECORD_FILE)
    }

    /// Where this agent's children live on disk. The container's NAME is
    /// declared config on the kernel (`Kernel.childrenDir`, default `agents`),
    /// never hardcoded — python `Agent._children_dir`.
    public func childrenDir(_ name: String) -> URL {
        rootPath.appendingPathComponent(name)
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

    /// Direct child ids in INSERTION (creation) order — python
    /// `child_ids()` returns `list(self._children.keys())`, and the tree,
    /// `tree=ids` and every descendant walk read that order.
    public func childIds() -> [AgentId] {
        childrenLock.lock()
        defer { childrenLock.unlock() }
        return Array(_children.keys)
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

    // ── in-flight dispatches ────────────────────────────────────

    /// How many dispatches are running on THIS agent right now — python's
    /// `_in_flight`. Bumped at dispatch start, dropped in the `finally`; it is
    /// what a state event reports as `backlog`, and what `state_snapshot`
    /// reports per agent. NOT a queue length: the kernel dispatches
    /// concurrently, so the number a reader wants is the work in hand.
    public var inFlight: Int {
        inFlightLock.lock()
        defer { inFlightLock.unlock() }
        return _inFlight
    }

    /// Enter a dispatch (python's `self._in_flight += 1`).
    func inFlightEnter() {
        inFlightLock.lock()
        _inFlight += 1
        inFlightLock.unlock()
    }

    /// Leave a dispatch — python's `max(0, self._in_flight - 1)`, clamped so an
    /// unbalanced leave can never drive the count negative.
    func inFlightLeave() {
        inFlightLock.lock()
        _inFlight = max(0, _inFlight - 1)
        inFlightLock.unlock()
    }
}
