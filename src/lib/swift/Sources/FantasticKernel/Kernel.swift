// Kernel — the substrate's central actor.
//
// Mirrors Rust's `fantastic_kernel::Kernel`. Owns the flat agents
// map (routing table), per-agent inbox channels, state event
// subscribers, the bundle registry, and the storage mode.
//
// `send(target, payload)` → resolves target → dispatches via system
// verb table OR the agent's bundle → publishes state event +
// watcher fanout → returns reply.
//
// `emit(target, payload)` → appends to target's inbox + publishes
// state event + fans out to watchers. No dispatch.

import FantasticJSON
import Foundation
import OrderedCollections

// ── Current sender task-local ─────────────────────────────────────

/// Task-local that names the agent currently dispatching. Read by
/// `Kernel.send` to attribute state events; written by `send_json_as`
/// + by `Kernel.send` itself before invoking the bundle handler
/// (so nested sends attribute to their target). Mirrors Rust's
/// `CURRENT_SENDER` task-local.
public enum KernelTaskLocals {
    @TaskLocal public static var currentSender: AgentId? = nil
}

/// Subscriber token returned by `Kernel.subscribe`; pass to
/// `Kernel.unsubscribe` to detach.
public struct SubscriberToken: Sendable, Hashable {
    public let value: UInt64
    init(_ value: UInt64) { self.value = value }
}

public typealias StateSubscriber = @Sendable (JSON) -> Void

public final class Kernel: @unchecked Sendable {
    /// The flat agents map. Lock-protected; readers snapshot via
    /// `agent(id:)`. Mirrors Rust's `DashMap<AgentId, Arc<Agent>>`.
    private let agentsLock = NSLock()
    private var _agents: [AgentId: Agent] = [:]

    /// Per-agent inbox stream-continuations. `emit` appends to
    /// these; watchers fan out by reading their own inbox stream.
    private let inboxLock = NSLock()
    private var _inboxes: [AgentId: AsyncStream<JSON>.Continuation] = [:]
    private var _inboxStreams: [AgentId: AsyncStream<JSON>] = [:]

    /// State subscribers — opaque tokens + callback closures.
    private let subscriberLock = NSLock()
    private var _subscribers: [(SubscriberToken, StateSubscriber)] = []
    private var nextSubscriberId: UInt64 = 1

    /// Atomic root agent reference. Set by `setRoot`; read by `root`.
    private let rootLock = NSLock()
    private var _root: Agent?

    /// HTTP port the listener is bound to (0 if no listener). Set
    /// by phase 8B when the Hummingbird listener binds. Exposed via
    /// `httpPort()` shim in `PublicAPI.swift`.
    let httpPortLock = NSLock()
    var _httpPort: UInt16 = 0

    public let bundles: BundleRegistry
    public let storage: StorageMode
    public let inboxBound: Int

    public init(
        storage: StorageMode = .inMemory,
        bundles: BundleRegistry = BundleRegistry(),
        inboxBound: Int = 256
    ) {
        self.storage = storage
        self.bundles = bundles
        self.inboxBound = inboxBound
    }

    // ── Agent map ───────────────────────────────────────────────

    /// Register `agent` in the flat routing map and create its inbox
    /// stream. Returns the stream the caller can iterate to read
    /// inbox messages. Mirrors `Rust Kernel::register` (which returns
    /// `mpsc::Receiver<Value>`).
    @discardableResult
    public func register(_ agent: Agent) -> AsyncStream<JSON> {
        agentsLock.lock()
        _agents[agent.id] = agent
        agentsLock.unlock()
        return ensureInbox(agent.id)
    }

    /// Snapshot the registered agent for `id`, if any.
    public func agent(_ id: AgentId) -> Agent? {
        agentsLock.lock()
        defer { agentsLock.unlock() }
        return _agents[id]
    }

    /// All registered agents in insertion order.
    public func allAgents() -> [Agent] {
        agentsLock.lock()
        defer { agentsLock.unlock() }
        return Array(_agents.values)
    }

    /// Remove an agent from the routing map + drop its inbox.
    /// Used by cascade delete.
    func unregister(_ id: AgentId) {
        agentsLock.lock()
        _agents[id] = nil
        agentsLock.unlock()
        inboxLock.lock()
        _inboxes[id]?.finish()
        _inboxes[id] = nil
        _inboxStreams[id] = nil
        inboxLock.unlock()
    }

    public func setRoot(_ agent: Agent) {
        rootLock.lock()
        _root = agent
        rootLock.unlock()
    }

    public var root: Agent? {
        rootLock.lock()
        defer { rootLock.unlock() }
        return _root
    }

    // ── Inbox + watchers ────────────────────────────────────────

    @discardableResult
    public func ensureInbox(_ id: AgentId) -> AsyncStream<JSON> {
        inboxLock.lock()
        defer { inboxLock.unlock() }
        if let stream = _inboxStreams[id] {
            return stream
        }
        var continuation: AsyncStream<JSON>.Continuation!
        let stream = AsyncStream<JSON>(
            bufferingPolicy: .bufferingNewest(inboxBound)
        ) { c in
            continuation = c
        }
        _inboxes[id] = continuation
        _inboxStreams[id] = stream
        return stream
    }

    func deliverToInbox(_ id: AgentId, _ payload: JSON) {
        inboxLock.lock()
        let continuation = _inboxes[id]
        inboxLock.unlock()
        if let continuation = continuation {
            _ = continuation.yield(payload)
        } else {
            // Auto-vivify for synthetic ids (browser clients), then deliver.
            _ = ensureInbox(id)
            inboxLock.lock()
            let c = _inboxes[id]
            inboxLock.unlock()
            _ = c?.yield(payload)
        }
    }

    /// Register `watcherId` as an observer of `srcId`'s inbox.
    public func watch(src: AgentId, watcher: AgentId) {
        if let src = agent(src) {
            src.addWatcher(watcher)
        }
        ensureInbox(watcher)
    }

    public func unwatch(src: AgentId, watcher: AgentId) {
        if let src = agent(src) {
            src.removeWatcher(watcher)
        }
    }

    func fanoutToWatchers(_ target: Agent, _ payload: JSON) {
        for w in target.watcherIds() {
            deliverToInbox(w, payload)
        }
    }

    // ── State events ────────────────────────────────────────────

    @discardableResult
    public func subscribe(_ cb: @escaping StateSubscriber) -> SubscriberToken {
        subscriberLock.lock()
        let id = nextSubscriberId
        nextSubscriberId += 1
        let token = SubscriberToken(id)
        _subscribers.append((token, cb))
        subscriberLock.unlock()
        return token
    }

    public func unsubscribe(_ token: SubscriberToken) {
        subscriberLock.lock()
        defer { subscriberLock.unlock() }
        _subscribers.removeAll { $0.0 == token }
    }

    func publishState(_ event: JSON) {
        subscriberLock.lock()
        let subs = _subscribers
        subscriberLock.unlock()
        for (_, cb) in subs {
            cb(event)
        }
    }
}
