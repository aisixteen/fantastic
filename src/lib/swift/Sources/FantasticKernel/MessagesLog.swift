// The message store on disk — cold hydration and the append pump.
//
// A port of python's `kernel_state/tools.py` (`read_messages`,
// `hydrate_messages`, `_MessagePump`), which is canonical; rust
// `fantastic-kernel/src/messages_log.rs`. The format is `MessagesXML`.
//
// THE KERNEL NEVER KNOWS DISK, here as there. It starts empty, always: this
// module reads the log FROM ABOVE and fills the store with `rows.put`, and the
// pump tails `rows.subscribe` and appends. Both are optional — no store wired
// means the log stays in RAM, which is the same law records live under, not a
// fallback.
//
// ORDER, BY CONTRACT: hydrate (cold, before any agent exists) → subscribe at
// the highest id present → only then create children. Subscribing before
// hydrating would re-append what was just read; creating children before
// subscribing would lose their first messages. `subscribe(after:)` REPLAYS
// everything above the cursor, so capturing `after` at hydration time covers
// anything minted before the pump starts.

import FantasticJSON
import Foundation
import OrderedCollections

/// The log, under the workdir's `.fantastic/`.
public let MESSAGES_FILE = "messages.xml"

/// The two ways the cold read refuses. Nothing is skipped or repaired.
public enum MessagesLogError: Error, CustomStringConvertible {
    case version(String)
    case row(String)
    public var description: String {
        switch self {
        case .version(let s), .row(let s): return s
        }
    }
}

/// COLD HYDRATION of the message store — `(rows, nextId)`.
///
/// Read before any agent exists, like the record tree. A file of another
/// version, or a cut row, is a LOUD error — nothing is skipped or repaired.
public func readMessages(rootDir: URL) throws -> ([Row], Int64) {
    let path = rootDir.appendingPathComponent(MESSAGES_FILE)
    guard FileManager.default.fileExists(atPath: path.path) else { return ([], 1) }
    let text = try String(contentsOf: path, encoding: .utf8)
    let head = text.split(separator: "\n", maxSplits: 1, omittingEmptySubsequences: false).first.map(String.init) ?? ""
    let want = MessagesXML.header.trimmingCharacters(in: .whitespacesAndNewlines)
    if head.trimmingCharacters(in: .whitespacesAndNewlines) != want {
        throw MessagesLogError.version(
            "\(MESSAGES_FILE): unknown log version — first line is \(pyRepr(head)), this loader writes \(pyRepr(want))")
    }
    let rows = try MessagesXML.parseMessages(text).filter { $0["id"]?.asInt != nil }
    let top = rows.compactMap { $0["id"]?.asInt }.max() ?? 0
    return (rows, top + 1)
}

/// Apply `readMessages` to a kernel FROM ABOVE, before its first mint.
///
/// Sets the counter FIRST, so every hydrated id is a pre-minted one. Returns
/// the row count. A duplicate id is the caller ingesting the same file twice
/// and is said loudly, exactly as `rows.put` says it.
@discardableResult
public func hydrateMessages(_ kernel: Kernel, rootDir: URL) throws -> Int {
    let (rows, nextId) = try readMessages(rootDir: rootDir)
    kernel.rows.setNextId(nextId)
    var settles: [(Int64, JSON)] = []
    for row in rows {
        guard let agent = row["agent"]?.asString else {
            throw MessagesLogError.row("\(MESSAGES_FILE): a row carries no `agent`")
        }
        if row["kind"]?.asString == "settle", let parent = row["parent"]?.asInt, let out = row["outcome"], out != .null {
            settles.append((parent, out))
        }
        try kernel.rows.put(AgentId(agent), row)
    }
    // REPLAY THE ENDINGS onto the calls they ended. A call comes back off disk
    // as it was WRITTEN — at launch, with no outcome — because a row is
    // append-only and the settle is its own append. Stamping here rebuilds the
    // state a live kernel had, and it is the only reason a cold read can
    // answer "how did this end" for a call that settled after the pump's
    // debounce (which is most of them: every AI pass, every job).
    for (parent, out) in settles {
        kernel.rows.stamp(parent, ["outcome": out])
    }
    return rows.count
}

/// PERSIST THE MESSAGE STORE — a tail of the kernel, appended to the provider.
///
/// The kernel exposes `rows.subscribe(after:)`; this pump is one consumer of
/// it. Rows are batched (0.15 s debounce) and each batch is ONE `write_stream`
/// append through the discovered provider (`findStore`) — no held-open channel
/// exists on the stream verbs, and one send per row would be a call row per
/// row. THE PUMP SKIPS ITS OWN CALLS: a flush is a send, so it mints a `call`
/// row on the provider; persisting that row would mint another, forever. The
/// rule is general: operator-context calls ON THE STORE (the loader's own
/// persistence traffic) and the pump's own `task` rows are bookkeeping of
/// persisting and never enter the log.
///
/// No provider ⇒ batches are dropped and said so once (RAM only, by design —
/// the same law as records).
public final class MessagePump: @unchecked Sendable {
    public static let verbs = ["message_pump", "message_pump_flush"]
    static let debounceNanos: UInt64 = 150_000_000

    private let kernel: Kernel
    private let lock = NSLock()
    private var pending: [Int64] = []
    /// The highest id the tail has handed us.
    public private(set) var seen: Int64 = 0
    /// The highest id landed on the provider.
    public private(set) var written: Int64 = 0
    private var saidNoStore = false
    /// Unknown until the first flush asks the store whether the log exists: a
    /// NEW log opens with the root element, an existing one is appended to.
    private var headerNeeded: Bool? = nil
    private var tasks: [TaskId] = []
    private let wake: AsyncStream<Void>
    private let wakeUp: AsyncStream<Void>.Continuation

    /// Sync-scoped locking — the pump's state is touched from async contexts,
    /// where `NSLock.lock()` is refused; the closure never suspends.
    private func withLock<T>(_ body: () -> T) -> T {
        lock.lock()
        defer { lock.unlock() }
        return body()
    }

    init(kernel: Kernel) {
        self.kernel = kernel
        (wake, wakeUp) = AsyncStream<Void>.makeStream(bufferingPolicy: .bufferingNewest(1))
    }

    /// Subscribe at `after` — TWO keepalive kernel tasks, not bare `Task`s: in
    /// the table, `ps` shows the pump, and `shutdown_agents` neither waits for
    /// it (it never ends) nor cancels it — the pump has to be alive AFTER the
    /// shutdown verb to append what that verb just wrote.
    func start(after: Int64) {
        seen = after
        written = after
        let root = kernel.root?.id ?? AgentId("kernel")
        let tail = kernel.rows.subscribe(after: after)
        let reader = kernel.spawnTask(agent: root, verb: Self.verbs[0], keepalive: true) { [self] in
            for await id in tail.ids {
                if Task.isCancelled { return }
                // SKIPPED AT ARRIVAL, like python's `_read`: the pump's own
                // traffic never pends, or every flush would mint the rows that
                // make the next flush necessary and `catchUp` could never
                // converge. The ROW is still read at flush time (an outcome
                // stamped inside the debounce belongs in the line).
                let own = kernel.rows.getOne(id).map { skips($0, store: kernel.findStore()) } ?? false
                withLock {
                    seen = max(seen, id)
                    if !own { pending.append(id) }
                }
                if !own { wakeUp.yield() }
            }
        }
        let flusher = kernel.spawnTask(agent: root, verb: Self.verbs[1], keepalive: true) { [self] in
            for await _ in wake {
                if Task.isCancelled { return }
                try? await Task.sleep(nanoseconds: Self.debounceNanos)
                await flush()
            }
        }
        tasks = [reader, flusher]
    }

    /// Does this row stay out of the log? The pump's own task rows, the store's
    /// operator-context calls (the loader's own persistence traffic — no
    /// origin, because nobody sent them), and the settles of either: a settle
    /// follows its call, and persisting the ending of a skipped write would
    /// bring the recursion back through the other half.
    func skips(_ row: Row, store: AgentId?) -> Bool {
        let kind = row["kind"]?.asString
        let agent = row["agent"]?.asString
        if kind == "task", agent == kernel.root?.id.value {
            return Self.verbs.contains(row["verb"]?.asString ?? "")
        }
        if kind == "call", let store, agent == store.value {
            let origin = row["origin"] ?? .null
            return origin == .null
        }
        if kind == "settle", let parent = row["parent"]?.asInt, let p = kernel.rows.getOne(parent) {
            return skips(p, store: store)
        }
        return false
    }

    /// Land what pends: ONE append per batch. The row is READ AT FLUSH TIME,
    /// not when it was queued — an outcome stamped onto a `call` row inside
    /// the debounce window belongs in the line about to be written.
    public func flush() async {
        let batch: [Int64] = withLock {
            let b = pending
            pending = []
            return b
        }
        if batch.isEmpty { return }
        guard let store = kernel.findStore() else {
            if !saidNoStore {
                // FLUSHED, like python's `print(..., flush=True)`: stdout is
                // block-buffered on a pipe, and this line was missing from
                // every piped capture because the process was signalled before
                // the buffer drained.
                printFlushed("  [messages] no store wired — messages stay in RAM")
                saidNoStore = true
            }
            return
        }
        var lines = ""
        var top: Int64 = 0
        for id in batch {
            guard let row = kernel.rows.getOne(id), !skips(row, store: store) else { continue }
            lines += MessagesXML.rowToXml(row)
            top = max(top, id)
        }
        if lines.isEmpty { return }
        if headerNeeded == nil {
            // ON THE BINARY CHANNEL, like the write: `read_stream` is a binary
            // verb, and the text path would answer "error" for a file that is
            // sitting right there — re-writing the root mid-document.
            let (probe, _) = await kernel.storeBookkeepingBinary(
                store,
                .object([
                    "type": .string("read_stream"), "path": .string(MESSAGES_FILE),
                    "offset": .integer(0), "length": .integer(1),
                ]), Data())
            headerNeeded = probe["error"].asString != nil
        }
        if headerNeeded == true {
            // The root, once, at the head of a NEW log; never closed (a stream).
            lines = MessagesXML.header + lines
            headerNeeded = false
        }
        let (reply, _) = await kernel.storeBookkeepingBinary(
            store, .object(["type": .string("write_stream"), "path": .string(MESSAGES_FILE)]),
            Data(lines.utf8))
        // PYTHON TRUTHINESS, and python's own rendering. `res.get("error")`
        // decides, so an `error` of `""`, `0`, `[]` or `{}` is NOT a refusal;
        // a non-string one (a dict the leg composed) is, and prints as
        // `str(error)`. Reading only `.asString` let a refusal that answered
        // anything else look like a successful append and the batch was dropped.
        if pyTruthy(reply["error"]) {
            printFlushed("  [messages] append refused: \(pyStr(reply["error"]))")
            withLock { pending = batch + pending }  // nothing lost; retried next flush
            return
        }
        withLock { written = max(written, top) }
    }

    /// Land every row minted so far. Each flush mints one call row (skipped),
    /// so the loop converges when the tail has seen everything and nothing
    /// pends.
    public func catchUp() async {
        for _ in 0..<1000 {
            await Task.yield()
            let done: Bool = withLock { seen >= kernel.rows.nextId - 1 && pending.isEmpty }
            if done { return }
            await flush()
            try? await Task.sleep(nanoseconds: 10_000_000)
        }
    }

    /// The kernel's shutdown hook: land every message minted so far, then stop.
    public func stop() async {
        await catchUp()
        wakeUp.finish()
        for t in tasks { _ = await kernel.cancelTask(t) }
        tasks = []
    }
}

extension Kernel {
    /// THE PUMP SUBSCRIBES HERE: after hydration (the bootstrap did it, cold),
    /// before anything below creates or wakes children. Idempotent.
    @discardableResult
    public func installMessagePump(after: Int64) -> MessagePump {
        if let p = messagePump { return p }
        let p = MessagePump(kernel: self)
        messagePump = p
        p.start(after: after)
        return p
    }

    /// The shutdown hook's half: land every row, stop the pump, forget it.
    public func flushPendingMessages() async {
        guard let p = messagePump else { return }
        await p.stop()
        messagePump = nil
    }
}

/// `print(..., flush=True)` — stdout is block-buffered on a pipe, so a line the
/// operator is meant to see must not wait for the buffer. Routed through
/// `RuntimeLog.out` so a test can read what an operator would.
public func printFlushed(_ line: String) {
    RuntimeLog.out(line)
}
