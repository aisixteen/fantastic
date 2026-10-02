// `Bootstrap.awaitStop` — the daemon's race between a signal and `kernel_stop` —
// and the `StopSignals` it waits on.
//
// Three endings, one suite, SERIALIZED: one test sends SIGTERM to this process,
// and every installed `StopSignals` sees a process signal, so a parallel
// neighbour would read it as its own.
//
//   - a SIGTERM that lands after `install` and before anyone waits is HELD for
//     the wait (the daemon installs before `[kernel] up`; rust's
//     `a_sigterm_before_the_wait_is_held_not_lost`);
//   - `kernel_stop` resolves the race with NO signal name (`nil`), so the daemon
//     prints no `[kernel] <SIG> — shutting down...` line for it;
//   - cancellation returns (C85).
//
// A wait runs DETACHED and is polled against a deadline, so a hang fails the
// test instead of stalling the whole suite (a task group would wait forever on
// a child that never returns).

import FantasticJSON
import FantasticKernel
import FantasticKernelStartup
import Foundation
import Testing

#if canImport(Darwin)
    import Darwin
#endif

private final class Answer: @unchecked Sendable {
    private let lock = NSLock()
    private var done = false
    private var value: String?
    func set(_ v: String?) {
        lock.withLock {
            done = true
            value = v
        }
    }
    var result: (done: Bool, value: String?) { lock.withLock { (done, value) } }
}

private func rootedKernel() -> Kernel {
    let kernel = Kernel(storage: .inMemory, bundles: BundleRegistry())
    let root = Agent(id: AgentId(ROOT_ID), handlerModule: nil, parentId: nil)
    kernel.register(root)
    kernel.setRoot(root)
    return kernel
}

private func settle(_ answer: Answer, within seconds: TimeInterval) async {
    let end = Date().addingTimeInterval(seconds)
    while !answer.result.done && Date() < end {
        try? await Task.sleep(nanoseconds: 10_000_000)
    }
}

@Suite("Bootstrap.awaitStop and StopSignals", .serialized)
struct AwaitStopPinTests {

    @Test func aSigtermBeforeTheWaitIsHeldNotLost() async {
        let kernel = rootedKernel()
        let signals = StopSignals.install()
        // A process-directed signal, as the reader of `[kernel] up` sends it.
        #expect(kill(getpid(), SIGTERM) == 0)
        try? await Task.sleep(nanoseconds: 50_000_000)
        let answer = Answer()
        Task.detached { answer.set(await Bootstrap.awaitStop(kernel: kernel, signals: signals)) }
        await settle(answer, within: 5)
        #expect(answer.result.done, "the SIGTERM sent before the wait was lost")
        #expect(answer.result.value == "SIGTERM")
    }

    @Test func kernelStopEndsTheRaceWithNoSignalName() async {
        let kernel = rootedKernel()
        let signals = StopSignals.install()
        let answer = Answer()
        Task.detached { answer.set(await Bootstrap.awaitStop(kernel: kernel, signals: signals)) }
        // let the wait park on both legs before the stop arrives
        try? await Task.sleep(nanoseconds: 200_000_000)
        let ack = await kernel.send([
            Item(AgentId("kernel"), .object(["type": .string("kernel_stop")]))
        ])[0].result
        #expect(ack.serialize() == #"{"type":"kernel_stop","ok":true}"#)
        await settle(answer, within: 3)
        #expect(answer.result.done, "awaitStop did not return after kernel_stop")
        #expect(
            answer.result.value == nil,
            "kernel_stop is not a signal: \(String(describing: answer.result.value))")
    }

    /// C85 — `shutdownRequested()` resolves on CANCELLATION as well as on the
    /// stop signal. It parked on a bare continuation, so the loser of the
    /// daemon's race was never resumed, `cancelAll()` waited forever, and the
    /// process had to be SIGKILLed with `lock.yaml` left behind.
    @Test func awaitStopReturnsWhenCancelled() async throws {
        let kernel = rootedKernel()
        let signals = StopSignals.install()
        let returned = await withTaskGroup(of: Bool.self) { group in
            group.addTask {
                _ = await Bootstrap.awaitStop(kernel: kernel, signals: signals)
                return true
            }
            group.addTask {
                // The OTHER leg of the daemon's race finishes first.
                try? await Task.sleep(nanoseconds: 50_000_000)
                return false
            }
            let first = await group.next() ?? false
            group.cancelAll()
            // The cancelled `awaitStop` must now RETURN — this await is the
            // test: it hung forever before.
            await group.waitForAll()
            return first
        }
        #expect(returned == false, "the signal leg should not have fired")
    }
}
