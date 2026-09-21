// The agent inbox element — a payload and, when it carries raw bytes, its body.
//
// Python queues the payload DICT itself, so a `write_stream`'s `bytes` value
// rides the inbox like any other field and a drainer hands the queued dict
// straight to `encode_frame`. Swift's `JSON` cannot hold raw `Data` (nor can
// rust's `serde_json::Value`), so the carrier splits: `payload` is the head,
// with `null` where the bytes lived, and `body` is `(path, data)` — the dotted
// path WITHIN the payload, which is exactly what `_binary_path` names once a
// drainer nests the payload inside its own envelope.
//
// Without this half no watcher could ever receive the binary frame python sends
// it: the fanout carried the header alone and the bytes were dropped on the
// floor.

import FantasticJSON
import Foundation

/// `(dotted path within a payload, raw bytes)` — the body half of a payload
/// that carries bytes.
public typealias InboxBody = (path: String, data: Data)

/// ONE queued payload — the whole payload, bytes included.
public struct InboxItem: Sendable {
    /// The payload dict. For a binary payload the bytes field is `null` here.
    public let payload: JSON
    /// The raw bytes and where in `payload` they lived, or `nil` for text.
    public let body: InboxBody?

    public init(payload: JSON, body: InboxBody?) {
        self.payload = payload
        self.body = body
    }

    /// A text payload — no bytes anywhere in it.
    public static func text(_ payload: JSON) -> InboxItem {
        InboxItem(payload: payload, body: nil)
    }

    /// A payload whose `path` field held `data`; `payload` carries `null` there.
    public static func binary(_ payload: JSON, _ path: String, _ data: Data) -> InboxItem {
        InboxItem(payload: payload, body: (path: path, data: data))
    }
}

/// ONE agent inbox — a lock-guarded FIFO with a NON-BLOCKING take, plus a
/// parking spot for the reader. `for await item in inbox` still reads it like
/// the `AsyncStream` it replaces (it is an `AsyncSequence`).
///
/// WHY NOT `AsyncStream`. Python's ws face is ordered by construction: the
/// fanout queues a watcher's payload BEFORE the handler runs, and the single
/// asyncio loop puts that payload on the wire before the handler's reply. A
/// parallel drain task cannot promise that — the reply, written from the call
/// task, overtook the event (the same race rust fixed in
/// `fantastic-kernel/src/inbox.rs`). The cure is a reader a CALL TASK can also
/// hold for a moment: it takes the same lock the drain holds, forwards
/// everything already queued, and only then writes its reply. `AsyncStream`
/// buries its buffer, so nothing but its own iterator can drain it, and the
/// iterator can only WAIT. This queue exposes `tryTake` (non-blocking) and
/// `ready` (wait with the lock RELEASED) — the two halves that split apart.
public final class Inbox: AsyncSequence, @unchecked Sendable {
    public typealias Element = InboxItem

    private let lock = NSLock()
    private var queue: [InboxItem] = []
    private var waiters: [CheckedContinuation<Void, Never>] = []
    private var closed = false
    /// python `bufferingNewest(inbox_bound)` — the OLDEST payload is dropped at
    /// the bound, so a stalled browser catches up on the freshest frames.
    private let bound: Int

    public init(bound: Int) {
        self.bound = bound
    }

    /// One SYNCHRONOUS critical section. `NSLock.lock()` is `noasync`, and
    /// every section here is a plain queue mutation — nothing under this lock
    /// ever suspends.
    private func sync<T>(_ body: () -> T) -> T {
        lock.lock()
        defer { lock.unlock() }
        return body()
    }

    /// Queue one payload. Called from the fanout, synchronously, under no
    /// assumption about the caller's thread.
    func put(_ item: InboxItem) {
        let woken: [CheckedContinuation<Void, Never>] = sync {
            if closed { return [] }
            if queue.count >= bound { queue.removeFirst() }
            queue.append(item)
            let w = waiters
            waiters = []
            return w
        }
        for c in woken { c.resume() }
    }

    /// The `AsyncStream.Continuation.finish` twin: the reader's loop ends once
    /// what is queued is read.
    func finish() {
        let woken: [CheckedContinuation<Void, Never>] = sync {
            closed = true
            let w = waiters
            waiters = []
            return w
        }
        for c in woken { c.resume() }
    }

    /// NON-BLOCKING take — `nil` when nothing is queued RIGHT NOW. This is what
    /// lets a writer flush the queue as one step with its own frame.
    public func tryTake() -> InboxItem? {
        sync { queue.isEmpty ? nil : queue.removeFirst() }
    }

    /// Wait until something is queued; `false` once the inbox is finished (or
    /// the waiting task is cancelled) and nothing is left.
    @discardableResult
    public func ready() async -> Bool {
        while true {
            let (has, done) = sync { (!queue.isEmpty, closed) }
            if has { return true }
            if done || Task.isCancelled { return false }
            await park()
        }
    }

    /// Take the next payload, waiting for one. `nil` ends the reader's loop.
    public func next() async -> InboxItem? {
        while true {
            if let item = tryTake() { return item }
            if await ready() == false { return nil }
        }
    }

    /// Park until `put`/`finish` wakes us, or this task is cancelled — a
    /// cancelled `for await` must END, as `AsyncStream`'s did.
    private func park() async {
        await withTaskCancellationHandler {
            await withCheckedContinuation { (c: CheckedContinuation<Void, Never>) in
                let parked: Bool = sync {
                    if closed || !queue.isEmpty || Task.isCancelled { return false }
                    waiters.append(c)
                    return true
                }
                if !parked { c.resume() }
            }
        } onCancel: {
            let woken: [CheckedContinuation<Void, Never>] = sync {
                let w = waiters
                waiters = []
                return w
            }
            for c in woken { c.resume() }
        }
    }

    public struct AsyncIterator: AsyncIteratorProtocol {
        let inbox: Inbox
        public mutating func next() async -> InboxItem? { await inbox.next() }
    }

    public func makeAsyncIterator() -> AsyncIterator { AsyncIterator(inbox: self) }
}
