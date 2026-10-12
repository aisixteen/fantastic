// A PAIRED in-process bridge transport for tests — python `MemoryTransport.pair()`
// through the real wire codec. Shared by the bridge-leg tests.

import FantasticIoBridge
import FantasticJSON
import FantasticKernel
import FantasticKernelBridge
import Foundation


/// One end of a PAIRED in-process transport — python `MemoryTransport.pair()`:
/// what one end sends, the other receives, through the real wire codec (a text
/// frame is serialized and re-parsed strictly; a binary one is encoded
/// `[4B len|header|body]` and decoded) — where `MemoryTransport.pair()` hands
/// each frame across as it is. Both ends are driven by the bridge ENGINE.
final class PairEnd: BridgeTransport, @unchecked Sendable {
    private let lock = NSLock()
    private var queue: [(JSON, Data, Bool)] = []
    private var waiter: CheckedContinuation<(JSON, Data, Bool)?, Never>?
    private var closed = false
    fileprivate var peer: PairEnd?

    static func pair() -> (PairEnd, PairEnd) {
        let a = PairEnd()
        let b = PairEnd()
        a.peer = b
        b.peer = a
        return (a, b)
    }

    private func isShut() -> Bool { lock.withLock { closed } }

    func isClosed() async -> Bool { isShut() }

    func send(_ frame: JSON) async throws {
        if isShut() { throw ConnectionClosed("pair transport closed") }
        peer?.deliver((try JSON.parseStrict(Codec.encodeText(frame)), Data(), false))
    }

    func sendBinary(header: JSON, body: Data) async throws {
        if isShut() { throw ConnectionClosed("pair transport closed") }
        let (h, b) = try Codec.decodeBinaryFrame(Codec.encodeBinaryFrame(header: header, body: body))
        peer?.deliver((h, b, true))
    }

    fileprivate func deliver(_ f: (JSON, Data, Bool)) {
        let w: CheckedContinuation<(JSON, Data, Bool)?, Never>? = lock.withLock {
            if closed { return nil }
            if let w = waiter {
                waiter = nil
                return w
            }
            queue.append(f)
            return nil
        }
        w?.resume(returning: f)
    }

    func recv() async throws -> (frame: JSON, body: Data, isBinary: Bool) {
        let next: (JSON, Data, Bool)? = await withCheckedContinuation { c in
            lock.lock()
            if !queue.isEmpty {
                let head = queue.removeFirst()
                lock.unlock()
                c.resume(returning: head)
                return
            }
            if closed {
                lock.unlock()
                c.resume(returning: nil)
                return
            }
            waiter = c
            lock.unlock()
        }
        guard let next else { throw ConnectionClosed("pair transport closed") }
        return next
    }

    fileprivate func shut() {
        let w: CheckedContinuation<(JSON, Data, Bool)?, Never>? = lock.withLock {
            closed = true
            let w = waiter
            waiter = nil
            return w
        }
        w?.resume(returning: nil)
    }

    func close() async {
        shut()
        peer?.shut()
    }
}
