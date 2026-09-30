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

    /// THE QUALIFIED REMOTE CALLER, as a bridge leg marked it: a call that
    /// crossed a bridge arrives carrying its sender's address `<remote kernel
    /// guid>|<remote agent id>` on the frame, and the inbound leg dispatches it
    /// under `withRemoteCaller(leg:address:)`. The dispatch that `send` launches
    /// CONSUMES the pair (python `_dispatch_task`): when the leg itself is that
    /// dispatch's caller, the address becomes `currentRemote`; a send the handler
    /// makes itself carries no remote caller — its own writes are keyed under its
    /// own id. Python `_remote_caller` / `_current_remote`.
    @TaskLocal public static var remoteCaller: RemoteCaller? = nil

    /// The qualified `<guid>|<agent id>` of a caller on ANOTHER kernel, when this
    /// dispatch was relayed by a bridge leg that carried it; `nil` otherwise.
    /// Python `current_remote_caller()`.
    @TaskLocal public static var currentRemote: String? = nil

    /// Called once a batch item's dispatch has FANNED OUT (its `send` reached the
    /// target's watchers) — the turnstile `send` uses to start the items of one
    /// batch in INPUT ORDER. Python creates the dispatch tasks in order and each
    /// runs its prologue (in-flight count, fanout) before the next one starts;
    /// a task group starts its children in any order, so two items to one agent
    /// reached its watchers swapped (measured: parity `ctx/ws_events.json`, a
    /// `recall` + `context_status` batch, and `ai_paths` cycle). Cleared for the
    /// handler, so its own sends never touch the caller's turnstile.
    @TaskLocal static var onFanout: (@Sendable () -> Void)? = nil

    /// Called once a batch has LAUNCHED — every item has fanned out (or settled
    /// without a dispatch) — while the items still settle. A caller that must
    /// not proceed before its batch reached its targets sets it around that one
    /// `send` (ai_core's routing: python's event loop runs the route task up to
    /// its dispatches before the `send` that spawned it hands its reply back).
    /// Cleared for every handler, like `onFanout`.
    @TaskLocal public static var onBatchLaunched: (@Sendable () -> Void)? = nil

    /// Mark the enclosed sends as `leg` relaying `address`'s call. Python
    /// `remote_caller_context(leg_id, address)`.
    public static func withRemoteCaller<T>(
        leg: AgentId, address: String, _ body: () async -> T
    ) async -> T {
        await $remoteCaller.withValue(RemoteCaller(leg: leg, address: address)) { await body() }
    }
}

/// A bridge leg relaying a remote caller: the leg's own id and the caller's
/// qualified address.
public struct RemoteCaller: Sendable, Equatable {
    public let leg: AgentId
    public let address: String
    public init(leg: AgentId, address: String) {
        self.leg = leg
        self.address = address
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
    /// The last refused/failed persist write — `persistence.error` on the root
    /// reflect until a write lands again (python `agent._persist_error`, d2).
    let persistErrorLock = NSLock()
    var _persistError: OrderedDictionary<String, JSON>?

    /// HTTP port the listener is bound to (0 if no listener). Set when the
    /// listener binds; read via `httpPort()` in `PublicAPI.swift`.
    let httpPortLock = NSLock()
    var _httpPort: UInt16 = 0

    public let bundles: BundleRegistry
    public let storage: StorageMode
    public let inboxBound: Int

    /// This kernel's process-unique serial — what a side table keyed PER KERNEL
    /// uses. Never an address: a kernel dropped without `shutdown()` keeps its
    /// rows, and the next kernel allocated at the same address must not read them
    /// as its own. Monotonic from 1, minted under a lock at `init`.
    public let serial: UInt64
    private static let serialLock = NSLock()
    nonisolated(unsafe) private static var lastSerial: UInt64 = 0

    /// The TASK TABLE — every in-flight dispatch, keyed by a task id
    /// (`"<agent_id>:<n>"`, a pid). Tasks form a TREE through the send chain;
    /// the `ps` system verb reads it, `cancel` kills a subtree. See `Tasks.swift`.
    public let tasks = TaskTable()

    /// THE EXECUTION ROWS still in RAM — `call`, `settle`, `task`, `ack` rows not
    /// yet flushed to the kernel's memory store. See `RowStore`; the one way in
    /// is `rowsPut`.
    public let rows = RowStore()
    /// The loader's debounced RECORD writer, once installed (see
    /// `RecordWriter.swift`). Held here so a one-shot caller with no reference
    /// to the loader bundle can still drain it before the process exits.
    public var recordWriter: RecordWriter?

    /// THE EXECUTION LEDGER, process side — every execution still in RAM: in
    /// flight, or settled and not yet flushed. A flushed one lives on as its
    /// `call` + `settle` rows in the kernel's memory store; `ledger` is a
    /// projection over both. See `Ledger.swift`, `KernelStore.swift`.
    public let ledger = ExecutionLedger()

    /// THE ONE COUNTER. Every id in the kernel — a call (one per batch item), a
    /// forked task, a settle, a conversation row an AI agent writes to its store
    /// — is the next value of this int64. Kernel-wide and monotonic.
    ///
    /// PERSISTED BY BLOCK RESERVATION, beside `guid` in the ROOT RECORD: `load`
    /// reserves a block past the persisted bound (in memory only — the record is
    /// untouched, so save → load → save round-trips), `start` writes that bound
    /// into the root record (`next_id`), and a started kernel minting past it
    /// reserves and writes the next block. A reboot starts at the persisted
    /// bound, so ids never repeat; a crash skips at most one block. Python
    /// `Kernel.next_id` / `id_reserved` / `ID_BLOCK`.
    let counterLock = NSLock()
    var _nextId: Int64 = 1
    var _idReserved: Int64 = 1
    public static let ID_BLOCK: Int64 = 1000

    /// THE MEMORY STORE the kernel writes its execution rows to — the root
    /// record's `memory_store_id`, resolved by `start`. `nil` ⇒ not started: rows
    /// stay in RAM. Guarded by `storeLock`; the flush state beside it.
    let storeLock = NSLock()
    var _storeId: AgentId? = nil
    var _started = false
    var _flushWake: AsyncStream<Void>.Continuation? = nil
    var _flusherTaskId: TaskId? = nil
    /// The flush rows this kernel wrote (by id), and the calls that carried them:
    /// those calls' own rows are ALREADY RECORDED — the flush row is their record.
    var _ownFlushes: Set<Int64> = []
    var _ownFlushCalls: [Int64: Int64] = [:]
    /// How many `call` rows the store holds for the kernel (counted as flushed,
    /// re-read at every prune) — what the retention window is measured against.
    var _storedCalls = 0
    /// ONE FLUSH AT A TIME.
    let flushMutex = AsyncMutex()

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

    /// CONNECTED PEER KERNELS — guid → the route to it (owner, 2026-09-30). An
    /// agent on another kernel is addressed `<guid>|<agent id>`; when that guid is
    /// here, `send` hands the call to its route and the reply is the call's
    /// ordinary outcome. A TRANSPORT registers the peer it is connected to (the web
    /// ws leg when a page says `hello`) and unregisters it when the connection ends
    /// — the transport only carries frames; addressing is the kernel's. A guid that
    /// is not here is an unknown target like any other. See `Peers.swift`.
    let peersLock = NSLock()
    var _peers: [String: PeerRoute] = [:]

    public init(
        storage: StorageMode = .inMemory,
        bundles: BundleRegistry = BundleRegistry(),
        inboxBound: Int = 256
    ) {
        self.storage = storage
        self.bundles = bundles
        self.inboxBound = inboxBound
        self.serial = Kernel.serialLock.withLock {
            Kernel.lastSerial += 1
            return Kernel.lastSerial
        }
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

    /// Resolve an id the way `send` does: `"kernel"` → the root, else a
    /// literal id. `nil` when it names no live agent. Python
    /// `Agent._resolve_agent` / the inline resolve in `_launch`.
    public func resolveAgent(_ id: String) -> Agent? {
        if id.isEmpty { return nil }
        if id == "kernel" { return root }
        return agent(AgentId(id))
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

    /// The kernel's RUNNING DIR — the dir holding its root's address (its
    /// `.fantastic`). The ONE source of the disk clamp base (every file_bridge
    /// root resolves under it) and of store discovery; nothing reads the process
    /// cwd in its place (audit 2026-09-28, c6; python `Kernel.workdir`). No root,
    /// or a root that never named its address (`Agent`'s default
    /// `URL(fileURLWithPath: "")`) ⇒ nil, and the disk verbs refuse.
    public var workdir: URL? {
        guard let rp = root?.rootPath, !(rp.baseURL != nil && rp.relativePath == ".") else {
            return nil
        }
        return rp.deletingLastPathComponent()
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

    public func deliverToInbox(_ id: AgentId, _ item: InboxItem) {
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
    public func hasInbox(_ id: AgentId) -> Bool {
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
