// `Bootstrap.awaitStop` — the daemon's race between a signal and `kernel_stop`.
//
// `BootstrapTests.awaitStopReturnsWhenCancelled` pins the cancellation leg. The
// other non-signal ending is `kernel_stop`: the race resolves with NO signal
// name (`nil`), so the daemon prints no `[kernel] <SIG> — shutting down...`
// line for it.
//
// The wait runs DETACHED and is polled against a deadline, so a hang fails this
// test instead of stalling the whole suite (a task group would wait forever on
// a child that never returns).

import FantasticJSON
import FantasticKernel
import FantasticKernelStartup
import Foundation
import Testing

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

@Suite("Bootstrap.awaitStop — the kernel_stop leg")
struct AwaitStopPinTests {

    @Test func kernelStopEndsTheRaceWithNoSignalName() async {
        let kernel = rootedKernel()
        let answer = Answer()
        Task.detached { answer.set(await Bootstrap.awaitStop(kernel: kernel)) }
        // let the wait park on both legs before the stop arrives
        try? await Task.sleep(nanoseconds: 200_000_000)
        let ack = await kernel.send([
            Item(AgentId("kernel"), .object(["type": .string("kernel_stop")]))
        ])[0].result
        #expect(ack.serialize() == #"{"type":"kernel_stop","ok":true}"#)
        let end = Date().addingTimeInterval(3)
        while !answer.result.done && Date() < end {
            try? await Task.sleep(nanoseconds: 10_000_000)
        }
        #expect(answer.result.done, "awaitStop did not return after kernel_stop")
        #expect(
            answer.result.value == nil,
            "kernel_stop is not a signal: \(String(describing: answer.result.value))")
    }
}
