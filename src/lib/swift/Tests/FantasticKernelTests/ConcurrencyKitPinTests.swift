// The kernel's hand-written concurrency primitives, pinned by what they promise.
//
// `AsyncMutex` serves waiters FIFO; `StartSignal` delivers once whether it fires
// before or after the wait and ignores a second fire; the stop signal
// (`requestShutdown` / `shutdownRequested`) wakes EVERY waiter once, answers a
// late waiter at once, and lets a cancelled waiter go without the signal. These
// are the races a shared continuation-slot / one-shot / mutex kit has to keep.

import FantasticJSON
import Foundation
import Testing

@testable import FantasticKernel

/// An append-only, thread-safe log of who got the lock, in order.
private final class Order: @unchecked Sendable {
    private let lock = NSLock()
    private var seen: [Int] = []
    func add(_ i: Int) { lock.withLock { seen.append(i) } }
    var value: [Int] { lock.withLock { seen } }
}

/// Run `body`, or fail after `seconds` — a hang must fail the test, not stall
/// the suite.
private func within<T: Sendable>(
    _ seconds: Double, _ body: @escaping @Sendable () async -> T
) async -> T? {
    await withTaskGroup(of: T?.self) { group in
        group.addTask { await body() }
        group.addTask {
            try? await Task.sleep(nanoseconds: UInt64(seconds * 1_000_000_000))
            return nil
        }
        let first = await group.next() ?? nil
        group.cancelAll()
        return first
    }
}

@Suite("Concurrency kit — the kernel's primitives")
struct ConcurrencyKitPinTests {

    /// Waiters queued behind a held mutex acquire it in ARRIVAL order, one at a
    /// time.
    @Test func asyncMutexServesWaitersFifo() async {
        let m = AsyncMutex()
        let order = Order()
        await m.acquire()
        var tasks: [Task<Void, Never>] = []
        for i in 1...6 {
            tasks.append(
                Task {
                    await m.acquire()
                    order.add(i)
                    await m.release()
                })
            // let waiter i park before waiter i+1 arrives
            try? await Task.sleep(nanoseconds: 20_000_000)
        }
        #expect(order.value.isEmpty, "nobody may pass a held mutex")
        await m.release()
        for t in tasks { await t.value }
        #expect(order.value == [1, 2, 3, 4, 5, 6])
        // released with no waiter: the next acquire is immediate
        let free = await within(1) {
            await m.acquire()
            await m.release()
            return true
        }
        #expect(free == true)
    }

    @Test func startSignalFiredBeforeTheWaitDeliversAtOnce() async {
        let s = StartSignal()
        s.fire()
        s.fire()  // idempotent: a retry fans out again; only the first counts
        let returned = await within(1) {
            await s.wait()
            return true
        }
        #expect(returned == true)
        // a second wait after the fire also returns at once
        #expect(
            await within(1) {
                await s.wait()
                return true
            } == true)
    }

    @Test func startSignalWakesAWaiterThatParkedFirst() async {
        let s = StartSignal()
        let waiter = Task { () -> Bool in
            await s.wait()
            return true
        }
        try? await Task.sleep(nanoseconds: 30_000_000)
        s.fire()
        s.fire()
        #expect(await within(1) { await waiter.value } == true)
    }

    /// `kernel_stop` acks, then wakes EVERY parked stop waiter exactly once; a
    /// waiter that arrives after the stop returns at once.
    @Test func theStopSignalWakesEveryWaiterOnce() async {
        let kernel = makeKernel(withEcho: false)
        let woke = Order()
        var waiters: [Task<Void, Never>] = []
        for i in 1...3 {
            waiters.append(
                Task {
                    await kernel.shutdownRequested()
                    woke.add(i)
                })
        }
        try? await Task.sleep(nanoseconds: 30_000_000)
        #expect(woke.value.isEmpty)
        let ack = await kernel.send([Item("kernel", .object(["type": .string("kernel_stop")]))])[0]
            .result
        #expect(ack.serialize() == #"{"type":"kernel_stop","ok":true}"#)
        let parked = waiters
        let all = await within(3) {
            for w in parked { await w.value }
            return true
        }
        #expect(all == true, "a stop waiter was never woken")
        #expect(woke.value.sorted() == [1, 2, 3])
        // requestShutdown is idempotent and a late waiter does not park
        kernel.requestShutdown()
        #expect(
            await within(1) {
                await kernel.shutdownRequested()
                return true
            } == true)
    }

    /// A cancelled stop waiter returns without the signal, and a later stop
    /// still wakes the waiters that were not cancelled.
    @Test func aCancelledStopWaiterReturnsAndTheOthersStillWake() async {
        let kernel = makeKernel(withEcho: false)
        let cancelled = Task { () -> Bool in
            await kernel.shutdownRequested()
            return true
        }
        let kept = Task { () -> Bool in
            await kernel.shutdownRequested()
            return true
        }
        try? await Task.sleep(nanoseconds: 30_000_000)
        cancelled.cancel()
        #expect(await within(1) { await cancelled.value } == true, "a cancelled waiter must return")
        kernel.requestShutdown()
        #expect(await within(1) { await kept.value } == true)
    }
}
