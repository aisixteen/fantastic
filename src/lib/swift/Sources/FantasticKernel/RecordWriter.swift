// The RECORD writer + the ROOT LOADER's boot — python `kernel_state._FlushLoop`
// and `_boot`.
//
// Records do not persist at the verb any more. `create_agent` / `update_agent` /
// `delete_agent` publish a state event and return; the ROOT LOADER subscribes to
// that stream and writes, debounced 150 ms, through the DISCOVERED provider. That
// is what makes persistence a consumer of the kernel rather than a branch inside
// it — and it is why a record created before a store was wired still lands the
// moment one is.
//
// THE MACHINERY LIVES HERE, in the kernel, because the shipped
// `kernel_state.tools` bundle and the kernel's own tests drive exactly the same
// sequence; the bundle owns the VERB SURFACE, not a second copy of this. Python
// keeps these handles on the loader agent (`_fs_flush_loop`, `_message_pump`).

import FantasticJSON
import Foundation
import OrderedCollections

/// The debounced record writer: the state-stream subscription plus the keepalive
/// task that drains it.
///
/// KEEPALIVE, because the persistence writer must outlive the quiet phase of
/// `shutdownAgents` — it is what flushes what shutdown produced. A bare task left
/// every write it made rowed under the dispatch that booted the loader, which
/// ends immediately.
public final class RecordWriter: @unchecked Sendable {
    static let debounceNanos: UInt64 = 150_000_000

    private let kernel: Kernel
    private let lock = NSLock()
    /// Ordered set of ids to write (python's `dict[str, None]`).
    private var persist: [AgentId] = []
    /// id → dir to remove.
    private var forget: [(AgentId, URL)] = []
    /// id → dir cache, so a REMOVED agent's path is still known.
    private var paths: [AgentId: URL] = [:]
    private var token: SubscriberToken?
    private var task: TaskId?
    private let wake: AsyncStream<Void>
    private let wakeUp: AsyncStream<Void>.Continuation

    init(kernel: Kernel) {
        self.kernel = kernel
        (wake, wakeUp) = AsyncStream<Void>.makeStream(bufferingPolicy: .bufferingNewest(1))
    }

    /// Sync-scoped locking — the state is touched from async contexts, where
    /// `NSLock.lock()` is refused; the closure never suspends.
    private func withLock<T>(_ body: () -> T) -> T {
        lock.lock()
        defer { lock.unlock() }
        return body()
    }

    func start(loader: AgentId) {
        withLock {
            for a in kernel.allAgents() { paths[a.id] = a.rootPath }
        }
        token = kernel.subscribe { [weak self] evt, _ in
            self?.onState(evt)
        }
        task = kernel.spawnTask(agent: loader, verb: "persist_writer", keepalive: true) {
            [self] in
            for await _ in wake {
                if Task.isCancelled { return }
                try? await Task.sleep(nanoseconds: Self.debounceNanos)
                await flush()
            }
        }
    }

    private func onState(_ evt: JSON) {
        guard let aid = evt["agent_id"].asString, !aid.isEmpty else { return }
        let id = AgentId(aid)
        switch evt["kind"].asString {
        case "added", "updated":
            guard let a = kernel.agent(id), !a.ephemeral else { return }
            withLock {
                paths[id] = a.rootPath
                forget.removeAll { $0.0 == id }
                if !persist.contains(id) { persist.append(id) }
            }
        case "removed":
            let known: URL? = withLock {
                guard let path = paths.removeValue(forKey: id) else { return nil }
                persist.removeAll { $0 == id }
                forget.append((id, path))
                return path
            }
            if known == nil { return }
        default:
            return
        }
        wakeUp.yield()
    }

    /// Drain the dirty queues once — the test seam AND the loop's body (python
    /// `aflush`).
    ///
    /// NO PROVIDER ⇒ THE DIRTY SET SURVIVES. Python returns BEFORE clearing the
    /// queues when no store is wired, so everything created before the store
    /// exists lands the moment one is.
    public func flush() async {
        guard kernel.findStore() != nil else { return }
        let (ids, gone): ([AgentId], [(AgentId, URL)]) = withLock {
            let p = persist
            let f = forget
            persist = []
            forget = []
            return (p, f)
        }
        for id in ids {
            guard let a = kernel.agent(id), !a.ephemeral else { continue }
            await kernel.persistRecord(a)
            // EVERY DRAINED WRITE SEEDS THE SIDECARS (python
            // `_persist_via_store` → `_seed_readme_via_store`), not only the
            // create that first made the record — a record written before a
            // store was wired would otherwise never get its readme.
            if let hm = a.handlerModule, let readme = kernel.bundles.get(hm)?.readme {
                await kernel.seedReadmeViaStore(a, content: readme)
            }
        }
        for (_, path) in gone {
            await kernel.forgetPath(path)
        }
    }

    /// Unsubscribe, final flush, stop the task. A clean shutdown loses nothing.
    func stop() async {
        if let token { kernel.unsubscribe(token) }
        token = nil
        wakeUp.finish()
        if let task { _ = await kernel.cancelTask(task) }
        task = nil
        await flush()
    }
}

/// What the ROOT LOADER's `boot` installed — the handles a shutdown needs back.
public struct RootLoaderState: Sendable {
    /// The debounced record writer, when this loader watches (`watch != false`).
    public var writer: RecordWriter?
    /// The message pump — ROOT only.
    public var pump: MessagePump?
    public init(writer: RecordWriter? = nil, pump: MessagePump? = nil) {
        self.writer = writer
        self.pump = pump
    }
}

extension Kernel {
    /// Patch an agent's meta and PUBLISH `updated` — python `Kernel.update`.
    ///
    /// NO DISK WRITE: the event is what drives the loader to re-persist. A bundle
    /// that wrote the record itself bypassed the writer's dirty set, so the next
    /// drain did not know the record had moved.
    @discardableResult
    public func update(_ agentId: AgentId, _ patch: OrderedDictionary<String, JSON>)
        -> AgentRecord?
    {
        guard let target = agent(agentId) else { return nil }
        let rec = target.updateMeta(patch)
        publishState(
            .object([
                "agent_id": .string(agentId.value),
                "kind": .string("updated"),
                "changed": .array(patch.keys.map { .string($0) }),
            ]))
        return rec
    }

    /// Subscribe the loader to the kernel state stream and start the debounced
    /// record writer. Replaces the inline persist that used to ride
    /// `create_agent` / `update_agent` / `delete_agent`.
    @discardableResult
    public func installPersistWriter(loader: AgentId) -> RecordWriter {
        if let w = recordWriter { return w }
        let w = RecordWriter(kernel: self)
        recordWriter = w
        w.start(loader: loader)
        return w
    }

    /// Drain the record writer NOW. A ONE-SHOT drains synchronously: python's
    /// `call` mode boots the root and awaits `root._fs_flush_loop.aflush()`
    /// before the process exits — the debounce never ticks in that window.
    public func flushRecords() async {
        await recordWriter?.flush()
    }

    /// The loader's `boot`, python `kernel_state._boot`:
    ///
    /// 1. ROOT only — rehydrate the execution ledger, then install its flusher
    ///    (that order, so hydration never rewrites the file it just read), then
    ///    start the message pump at the highest id hydration saw.
    /// 2. Unless `watch=false` (a loader serving a REMOTE kernel answers verbs
    ///    only) — subscribe the record writer to the kernel state stream.
    /// 3. ROOT only — mint the instance `guid` ONCE, AFTER the writer subscribes,
    ///    so the `updated` event it publishes is what carries the new record to
    ///    disk. An explicit or already-persisted guid always wins, and once
    ///    written back it is never regenerated.
    public func bootRootLoader(_ loader: Agent) async -> RootLoaderState {
        var state = RootLoaderState()
        let isRoot = loader.parentId == nil
        if isRoot {
            await installLedgerPersistence()
            state.pump = installMessagePump(after: rows.nextId - 1)
        }
        let watching = loader.metaValue(forKey: "watch").map { $0 != .bool(false) } ?? true
        if !watching { return state }
        state.writer = installPersistWriter(loader: loader.id)
        if isRoot, (loader.metaValue(forKey: "guid")?.asString ?? "").isEmpty {
            update(loader.id, ["guid": .string(Kernel.mintGuid())])
        }
        return state
    }

    /// Land the last window and unsubscribe — python `_MessagePump.stop` +
    /// `_FlushLoop.stop`.
    public func shutdownRootLoader(_ state: RootLoaderState) async {
        if let w = state.writer {
            await w.stop()
            recordWriter = nil
        }
        if state.pump != nil { await flushPendingMessages() }
    }

    /// A 32-hex instance guid — the same spelling python's `uuid4().hex` writes.
    public static func mintGuid() -> String {
        UUID().uuidString.replacingOccurrences(of: "-", with: "").lowercased()
    }
}
