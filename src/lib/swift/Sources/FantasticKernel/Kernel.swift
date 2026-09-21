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

    /// THE KERNEL'S OWN SHUTDOWN TRAFFIC. `shutdownAgents` sets the drain flag
    /// FIRST (so nothing new enters while it works) and only then sends every
    /// advertiser its `shutdown` verb — which the drain gate would otherwise
    /// refuse as new work. Python `_in_shutdown` / `shutdown_context()`.
    @TaskLocal public static var inShutdown: Bool = false

    /// The row THIS Swift task runs under — stamped only by the table's own
    /// runners (`spawnTask`, `runDispatch`), never inherited as a parent link
    /// the way `currentTask` may be. `TaskTable.isRegistered` reads it: python
    /// compares the asyncio task object against every row's handle.
    @TaskLocal static var ownTask: TaskId? = nil

    /// Run `body` as the kernel's own shutdown traffic — exempt from the drain
    /// gate. Python `shutdown_context()`, rust `with_shutdown`.
    public static func withShutdown<T>(_ body: () async -> T) async -> T {
        await $inShutdown.withValue(true) { await body() }
    }
}

/// The tree root's id — python's `kernel_state`, which IS the root and carries
/// the durable-state loader's own `handler_module`. Not a default that may be
/// substituted: there is exactly one root id, and a persisted record that names
/// another one is the authority for THAT tree.
public let ROOT_ID = "kernel_state"

/// The root's `handler_module`: the durable-state loader bundle. A root record is
/// seeded with it on a virgin dir, exactly as python's `_bootstrap` does.
public let ROOT_HANDLER_MODULE = "kernel_state.tools"

/// Subscriber token returned by `Kernel.subscribe`; pass to
/// `Kernel.unsubscribe` to detach.
public struct SubscriberToken: Sendable, Hashable {
    public let value: UInt64
    init(_ value: UInt64) { self.value = value }
}

/// A subscriber callback fired on every state event (`send` / `drain` / `emit` /
/// `added` / `updated` / `removed`). Synchronous, called inline — keep it cheap.
///
/// The second argument is the event payload's RAW BODY — `(path within
/// `payload`, bytes)` — for a traffic event whose payload carried bytes. Python
/// queues the bytes inside the payload dict and its WS tap re-encodes the whole
/// event as a binary frame; `JSON` cannot hold raw `Data`, so the body rides
/// beside the event exactly as it rides beside an `InboxItem`. `nil` for every
/// event with no bytes.
public typealias StateSubscriber = @Sendable (JSON, InboxBody?) -> Void

public final class Kernel: @unchecked Sendable {
    /// The flat agents map, in REGISTRATION order — python's registry is a
    /// plain dict and `list_agents` walks `self.ctx.agents.values()` with no
    /// sort, so the order records come back in IS the order they were made.
    /// Lock-protected; readers snapshot via `agent(id:)`.
    private let agentsLock = NSLock()
    private var _agents: OrderedDictionary<AgentId, Agent> = [:]

    /// Declared aliases (an agent's `alias` meta → its id) — python's `well_
    /// known`. Wired at `register`, dropped at `unregister`. `send` targets,
    /// `create_agent parent_id` and the gate's placement operand all resolve
    /// through it, AFTER the built-in `kernel` alias (C35) — see
    /// [`resolveAgent`].
    private let wellKnownLock = NSLock()
    private var _wellKnown: [String: AgentId] = [:]

    /// The name of the container directory a parent keeps its children in.
    /// DECLARED CONFIG, read off the root record at hydration (`children_dir`),
    /// default `agents` — a self-describing tree may name it `host_agents`.
    /// Python `Kernel.children_dir`.
    public var childrenDir: String = "agents"

    /// Per-agent inbox stream-continuations. `emit` appends to
    /// these; watchers fan out by reading their own inbox stream.
    private let inboxLock = NSLock()
    private var _inboxes: [AgentId: Inbox] = [:]

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

    /// THE MESSAGE STORE — every message the kernel ever made, id-addressed by
    /// the one counter. See `RowStore`.
    public let rows = RowStore()
    /// The loader's message pump, once installed (see `MessagesLog.swift`).
    public var messagePump: MessagePump?
    /// The loader's debounced RECORD writer, once installed (see
    /// `RecordWriter.swift`). Held here so a one-shot caller with no reference
    /// to the loader bundle can still drain it before the process exits.
    public var recordWriter: RecordWriter?

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

    /// LIFECYCLE STATE — the methods live in `Lifecycle.swift`.
    /// DRAINING: the first phase of a graceful stop (`shutdownAgents`). New
    /// roots are refused `{reason:"draining"}`; sends with a live parent task,
    /// and the kernel's own `shutdown` verbs, still pass.
    let lifecycleLock = NSLock()
    var _draining = false
    /// `shutdown()` ran to the end — the SIGTERM path and any safety net
    /// collapse to one walk. Python `_shutdown_complete`.
    var _shutdownComplete = false
    /// The daemon's stop signal: `requestShutdown` flips it and wakes every
    /// `shutdownRequested()` waiter; a late waiter returns at once.
    var _shutdownRequested = false
    var _shutdownWaiters: [ShutdownWaitSlot] = []
    /// How long in-flight work may take to settle before it is cancelled. An
    /// instance value so a test can tighten it, exactly as python's test sets
    /// `k.QUIET_TIMEOUT`; the constant is `Kernel.QUIET_TIMEOUT`.
    var quietTimeout: Double = Kernel.QUIET_TIMEOUT

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

    /// Register `agent` in the flat routing map. NO INBOX.
    ///
    /// An agent does not get a queue merely by existing. `register` used to mint
    /// one and hand back the stream, which almost every caller dropped on the
    /// same line — so the queue was live, nobody read it, and `watch` happily
    /// mirrored into it forever. `ensureInbox` IS the reader registration, and
    /// every caller of it drains what it gets. Python `Kernel.register`.
    public func register(_ agent: Agent) {
        agentsLock.lock()
        _agents[agent.id] = agent
        agentsLock.unlock()
        // C35: a declared `alias` meta becomes addressable by that stable
        // name — like the built-in `kernel`→root alias, but for anything an
        // operator names. Declared config the substrate makes functional,
        // not autonomous behavior. Python `Kernel.register`.
        if case .string(let alias)? = agent.metaValue(forKey: "alias"), !alias.isEmpty {
            wellKnownLock.lock()
            _wellKnown[alias] = agent.id
            wellKnownLock.unlock()
        }
    }

    /// Compose an EPHEMERAL child directly under `parent` — python's `Cli
    /// (kernel, parent=...)` (`class Cli(Agent): ephemeral = True`), which
    /// bypasses `create_agent` entirely. `ephemeral` is a CONSTRUCTION-time
    /// flag no verb payload can set (a bundle has no distinct Swift class the
    /// way python's `Cli` subclass carries its own `ephemeral = True`), and
    /// the renderer is WIRED, never booted (A14.13/A14.15: the daemon fires
    /// `intro_booting`/`booted` at it directly). Publishes the same `added`
    /// state event `create_agent` does, so a watcher sees it exactly as it
    /// would any other child — the record-writer ignores it all the same
    /// (`!ephemeral` guards every persist queue).
    @discardableResult
    public func composeEphemeralChild(
        id: AgentId, handlerModule: String, parent: Agent,
        meta: OrderedDictionary<String, JSON> = [:]
    ) -> Agent {
        let child = Agent(
            id: id, handlerModule: handlerModule, parentId: parent.id, meta: meta,
            ephemeral: true)
        register(child)
        parent.insertChild(child)
        publishState(
            .object([
                "agent_id": .string(child.id.value), "kind": .string("added"),
                "name": .string(meta["display_name"]?.asString ?? id.value),
                "parent_id": .string(parent.id.value),
            ]))
        return child
    }

    /// Snapshot the registered agent for `id`, if any.
    public func agent(_ id: AgentId) -> Agent? {
        agentsLock.lock()
        defer { agentsLock.unlock() }
        return _agents[id]
    }

    /// Resolve an id the way `send` does (C35): `"kernel"` → the root, else
    /// through a declared `alias` (`well_known`), else a literal id. `nil`
    /// when it names no live agent. Python `Agent._resolve_agent` / the
    /// inline resolve in `_launch`.
    public func resolveAgent(_ id: String) -> Agent? {
        if id.isEmpty { return nil }
        if id == "kernel" { return root }
        wellKnownLock.lock()
        let real = _wellKnown[id]
        wellKnownLock.unlock()
        return agent(real ?? AgentId(id))
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
        _agents.removeValue(forKey: id)
        agentsLock.unlock()
        // Drop any alias pointing at it (C35). Python `Kernel.unregister`.
        wellKnownLock.lock()
        for (name, target) in _wellKnown where target == id { _wellKnown.removeValue(forKey: name) }
        wellKnownLock.unlock()
        inboxLock.lock()
        _inboxes[id]?.finish()
        _inboxes[id] = nil
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
    public func ensureInbox(_ id: AgentId) -> Inbox {
        inboxLock.lock()
        defer { inboxLock.unlock() }
        if let inbox = _inboxes[id] {
            return inbox
        }
        let inbox = Inbox(bound: inboxBound)
        _inboxes[id] = inbox
        return inbox
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
    }

    func deliverToInbox(_ id: AgentId, _ item: InboxItem) {
        inboxLock.lock()
        let inbox = _inboxes[id]
        inboxLock.unlock()
        // AN INBOX EXISTS IFF A READER REGISTERED IT. Delivery to an id nobody
        // holds a stream for is a no-op. This used to auto-vivify, which minted
        // a buffer for a consumer that never arrived: it filled to `inboxBound`
        // and then evicted one payload per message, forever, with nothing to say
        // so. `ensureInbox` IS the registration.
        if let inbox = inbox {
            inbox.put(item)
        }
    }

    /// Does `id` hold a registered inbox? A watcher without one receives neither
    /// the mail nor a state event — python's `if tgt_inbox is None: continue`.
    func hasInbox(_ id: AgentId) -> Bool {
        inboxLock.lock()
        defer { inboxLock.unlock() }
        return _inboxes[id] != nil
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
    /// Found 2026-08-27 in a production audit: a browser leg granted exactly
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
        guard hasInbox(watcher) else {
            return Refusal.make(
                Refusal.noConsumer,
                "watch: \(pyRepr(watcher.value)) has no registered inbox, so nothing "
                    + "would read what is mirrored there",
                hint: "register the reader first — ctx.ensure_inbox(\(pyRepr(watcher.value))) "
                    + "— then watch",
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

    /// THE ONE FANOUT — python's `Agent._fanout`, run BEFORE the handler and
    /// once per attempt.
    ///
    /// The target's OWN inbox first (a registered reader sees what was sent TO
    /// it, not only what its handler replied), then the state event for the
    /// target, then each watcher's inbox plus a state event per watcher that is a
    /// REAL agent. A synthetic watcher (a browser client id) gets the mail and no
    /// telemetry ray — publishing one would mint a phantom sprite.
    ///
    /// The whole `item` travels, bytes included: a watcher on a `write_stream`
    /// receives the body, which is the binary frame python sends it.
    func fanout(_ target: Agent, _ kind: String, _ item: InboxItem) {
        // THE CALLER, not the target. Read before any dispatch scope re-points
        // `currentSender` at the target; an operator send carries JSON null.
        let sender = KernelTaskLocals.currentSender
        let summary = summarize(item.payload, body: item.body)
        deliverToInbox(target.id, item)
        publishState(
            Kernel.trafficEvent(
                target.id, kind, target.inFlight, sender, summary, item.payload),
            body: item.body)
        for w in target.watcherIds() {
            guard hasInbox(w) else { continue }
            deliverToInbox(w, item)
            if let wa = agent(w) {
                publishState(
                    Kernel.trafficEvent(w, kind, wa.inFlight, sender, summary, item.payload),
                    body: item.body)
            }
        }
    }

    /// One traffic state event, in python's key order:
    /// `{agent_id, kind, backlog, sender, summary, payload}` (`ts` is stamped by
    /// `publishState`). `sender` is the originator or JSON null — never the
    /// target's own id, which is what swift used to substitute for an operator
    /// send.
    static func trafficEvent(
        _ agentId: AgentId, _ kind: String, _ backlog: Int, _ sender: AgentId?,
        _ summary: String, _ payload: JSON
    ) -> JSON {
        .object([
            "agent_id": .string(agentId.value),
            "kind": .string(kind),
            "backlog": .integer(Int64(backlog)),
            "sender": sender.map { JSON.string($0.value) } ?? .null,
            "summary": .string(summary),
            "payload": payload,
        ])
    }

    /// The inbox element for a BINARY dispatch: the header with `bytes` nulled,
    /// plus the blob at that same path. `bytes` is the key python's own payload
    /// dict holds the blob under, so a watcher receives the body — not a header
    /// with a hole where it was.
    static func binaryItem(_ header: JSON, _ blob: Data) -> InboxItem {
        var head = header
        if case .object(var o) = head {
            o["bytes"] = .null
            head = .object(o)
        }
        return .binary(head, "bytes", blob)
    }

    /// The `{agent_id, kind:"drain", backlog}` event every dispatch publishes
    /// once its handler has returned — python's `_dispatch` `finally`.
    static func drainEvent(_ agentId: AgentId, _ backlog: Int) -> JSON {
        .object([
            "agent_id": .string(agentId.value),
            "kind": .string("drain"),
            "backlog": .integer(Int64(backlog)),
        ])
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

    /// Push a state event to every subscriber. Keep callbacks short — they run
    /// on the dispatching task.
    ///
    /// `ts` is stamped HERE and LAST (python `{**event, "ts": time.time()}`), so
    /// every event carries the moment it was published and no caller has to
    /// remember to add one. No subscribers = no work at all, as in python.
    func publishState(_ event: JSON, body: InboxBody? = nil) {
        subscriberLock.lock()
        let subs = _subscribers
        subscriberLock.unlock()
        if subs.isEmpty { return }
        var stamped = event
        if case .object(var o) = event {
            o["ts"] = .double(Date().timeIntervalSince1970)
            stamped = .object(o)
        }
        for (_, cb) in subs {
            cb(stamped, body)
        }
    }

    /// Synchronous read of every loaded agent's identity + in-flight handler
    /// count — python's `Kernel.state_snapshot`. Used by a new `state_subscribe`
    /// subscriber to bootstrap its agent view before the first event arrives. No
    /// queue puts, no fanout.
    ///
    /// Each entry `{agent_id, name, backlog}`, in REGISTRATION ORDER (python
    /// walks `self.agents.values()`).
    public func stateSnapshot() -> [JSON] {
        allAgents().map { a in
            .object([
                "agent_id": .string(a.id.value),
                "name": .string(displayNameOrId(a)),
                "backlog": .integer(Int64(a.inFlight)),
            ])
        }
    }
}
