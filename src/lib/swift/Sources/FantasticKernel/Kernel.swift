// Kernel — the substrate's central actor.
//
// Mirrors Rust's `fantastic_kernel::Kernel`. Owns the flat agents
// map (routing table), per-agent inbox channels, state event
// subscribers, the bundle registry, and the storage mode.
//
// `send([Item(target, payload)])[0].result` → resolves target → dispatches via system
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

    /// The id of the TASK whose dispatch is running right now — the task-table
    /// twin of `currentSender`. Stamped INSIDE the spawned dispatch (an
    /// unstructured `Task {}` inherits its creator's task-locals, so each
    /// dispatch must overwrite it with its OWN id), read by the next `send` to
    /// record its PARENT. That is what makes the table a tree. `nil` outside any
    /// dispatch (the WS/REST surface, a one-shot CLI verb, a test) — such a task
    /// is a root of the table, like a process spawned by init.
    @TaskLocal public static var currentTask: TaskId? = nil

    /// The id that ORIGINATED the dispatch running right now — the CALLER, as
    /// the gate saw it, captured before ``currentSender`` is rewritten to the
    /// target's own id.  `nil` = an operator send.
    ///
    /// This exists because `currentSender` is the CALLEE mid-dispatch: inside a
    /// handler it reads as that handler's own agent, so a bundle could not tell
    /// who actually called it. That gap let an agent write ANOTHER agent's
    /// memory through a blob verb — the payload NAMES the namespace and nothing
    /// could check it against the caller.
    ///
    /// Read it when a verb's payload names a resource the caller might not own.
    /// Do NOT use it for `(sender, target, verb)` decisions — that is the gate's
    /// job, it runs first, and duplicating it here would be a second truth.
    @TaskLocal public static var currentOrigin: AgentId? = nil

    /// "My own task is inside a subtree the kernel is killing." Set by
    /// `Kernel.cancelTask` on every marked row BEFORE any task is stopped, so a
    /// handler woken by its child's death cannot register fresh work while its
    /// own kill is in flight. Mirrors Rust's `CURRENT_KILLED`.
    @TaskLocal public static var currentKilled: KilledFlag? = nil
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

    /// The TASK TABLE — every in-flight dispatch, keyed by a task id
    /// (`"<agent_id>:<n>"`, a pid). Tasks form a TREE through the send chain;
    /// the `ps` system verb reads it, `cancel` kills a subtree. See `Tasks.swift`.
    public let tasks = TaskTable()

    /// EVERY execution and what it became — named (a caller's dedup contract) or
    /// unnamed (under its minted pid). Session lifetime, retention-ringed; the
    /// DURABLE half is written through the discovered store, because the kernel
    /// never touches disk itself.
    public let ledger = ExecutionLedger()

    /// Installed by the boot seam once a store can be discovered. `nil` = no
    /// persistence (nothing wired, or a test kernel): settled outcomes live and
    /// die with the process, exactly like the tree's own auto-flush law.
    let ledgerFlusherLock = NSLock()
    var _ledgerFlusher: (@Sendable () -> Void)?

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

    /// Drop a SYNTHETIC inbox its owner created with `ensureInbox` — a relay's
    /// watch proxy, a WS client's row. Whoever vivified it must release it, or
    /// the continuation lingers forever and `emit` keeps buffering into a stream
    /// nobody drains (measured 2026-09-02: a spawned program that watched an
    /// agent left a `_xbw_…` row behind on every stop). NOT for an agent's own
    /// inbox — that belongs to `unregister`, which also removes the agent.
    public func removeInbox(_ id: AgentId) {
        inboxLock.lock()
        defer { inboxLock.unlock() }
        _inboxes[id]?.finish()
        _inboxes[id] = nil
        _inboxStreams[id] = nil
    }

    func deliverToInbox(_ id: AgentId, _ payload: JSON) {
        inboxLock.lock()
        let continuation = _inboxes[id]
        inboxLock.unlock()
        // AN INBOX EXISTS IFF A READER REGISTERED IT. Delivery to an id nobody
        // holds a stream for is a no-op. This used to auto-vivify, which minted
        // a buffer for a consumer that never arrived: it filled to `inboxBound`
        // and then evicted one payload per message, forever, with nothing to say
        // so. `ensureInbox` IS the registration.
        if let continuation = continuation {
            _ = continuation.yield(payload)
        }
    }

    /// Register `watcherId` as an observer of `srcId`'s inbox.
    /// Mirror `src`'s inbound traffic into `watcher`'s inbox. Returns a refusal
    /// when the gate denies it, else `nil`.
    ///
    /// GATED LIKE A SEND, because it IS a read of the target. A watcher receives
    /// src's raw inbound payloads VERBATIM — the model prompt, the document
    /// body, whatever was sent — so an ungated `watch` is a read side-channel
    /// around `callable_by`: an agent that may not CALL an agent could still
    /// read everything sent TO it.
    ///
    /// Found 2026-08-27 in a dataroom audit: a browser leg granted exactly
    /// `assistant:[send]` could mirror ANY agent's inbox in its session and read
    /// the packed confidential context out of the model's prompt. The `call`
    /// path went through the two-sided gate; this one never did.
    ///
    /// An OPERATOR watch (`currentSender` is nil) is the origin root of trust
    /// and stays ungated, as with send.
    @discardableResult
    public func watch(src: AgentId, watcher: AgentId) -> JSON? {
        guard let srcAgent = agent(src) else { return nil }
        if let orig = KernelTaskLocals.currentSender {
            if let denial = gate(sender: orig, verb: "watch", target: srcAgent, payload: .null) {
                return denial
            }
        }
        // A WATCHER MUST ALREADY BE A READER. This used to lazy-create the
        // watcher's inbox, so a caller could watch and never drain: the mirror
        // went into a buffer nobody read. Registering a consumer is what `watch`
        // requires, not what it does.
        let known: Bool = {
            inboxLock.lock()
            defer { inboxLock.unlock() }
            return _inboxes[watcher] != nil
        }()
        guard known else {
            return Refusal.make(
                Refusal.noConsumer,
                "watch: \(watcher.value) has no registered inbox, so nothing "
                    + "would read what is mirrored there",
                extra: [
                    ("watcher", .string(watcher.value)),
                    ("src", .string(src.value)),
                ])
        }
        srcAgent.addWatcher(watcher)
        return nil
    }

    /// May `watcher` tap the kernel-wide STATE STREAM? Refusal, or `nil`.
    ///
    /// The stream carries every send's sender, target, verb and payload summary
    /// for every agent in the tree, so subscribing is the broadest read the
    /// kernel offers. It is gated on the ROOT, and `state_subscribe` is a
    /// privileged verb so a wildcard never covers it: the edge must be named.
    ///
    /// An ingress rule answers "may this client reach this leg"; it says nothing
    /// about what the client may then READ.
    public func authorizeStateSubscribe(_ watcher: AgentId) -> JSON? {
        // An OPERATOR tap is the origin root of trust, as with send and watch.
        guard KernelTaskLocals.currentSender != nil else { return nil }
        guard let rootAgent = root else { return nil }
        return gate(sender: watcher, verb: "state_subscribe", target: rootAgent, payload: .null)
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
