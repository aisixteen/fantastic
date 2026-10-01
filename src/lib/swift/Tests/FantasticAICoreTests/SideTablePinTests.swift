// The AI core's process-wide side tables, pinned per KERNEL.
//
// Swift hosts many kernels in one process (every test does), so each table is
// keyed by `kernel.serial`: two kernels holding an agent with the same id must
// not share a calibration, an incarnation, a run state (its FIFO lock), the
// memory state, a silent streak or a silent-turn cap — and a kernel's teardown
// takes its rows with it. Also the FIFO generation lock (`AIFifoLock`) and the
// test seams the suite drives (`set`/`bump`/`setCap`/`has`/`forget`/`of`).

import FantasticJSON
import FantasticKernel
import Foundation
import Testing

@testable import FantasticAICore

private func bareKernel() -> Kernel {
    let k = Kernel(storage: .inMemory, bundles: BundleRegistry())
    let root = Agent(id: AgentId("kernel_state"), handlerModule: nil, parentId: nil)
    k.register(root)
    k.setRoot(root)
    return k
}

private final class Order: @unchecked Sendable {
    private let lock = NSLock()
    private var seen: [Int] = []
    func add(_ i: Int) { lock.withLock { seen.append(i) } }
    var value: [Int] { lock.withLock { seen } }
}

@Suite("AI side tables — keyed by kernel")
struct SideTablePinTests {

    @Test func twoKernelsWithTheSameAgentIdShareNothing() {
        let a = bareKernel()
        let b = bareKernel()
        #expect(a.serial != b.serial)

        AICalibration.set(a, "ai", 10.0)
        #expect(AICalibration.calibration(a, "ai") == 10.0)
        #expect(AICalibration.calibration(b, "ai") == 1.0)
        AICalibration.record(a, "ai", reported: 500, estimate: 250)
        #expect(AICalibration.calibration(a, "ai") == 2.0)
        #expect(AICalibration.reportedTokens(a, "ai") == 500)
        #expect(AICalibration.reportedTokens(b, "ai") == nil)
        // an estimate of 0 divides by 1, never by 0
        AICalibration.record(b, "ai", reported: 7, estimate: 0)
        #expect(AICalibration.calibration(b, "ai") == 7.0)
        // set(nil) forgets BOTH the ratio and the report
        AICalibration.set(a, "ai", nil)
        #expect(AICalibration.calibration(a, "ai") == 1.0)
        #expect(AICalibration.reportedTokens(a, "ai") == nil)
        #expect(AICalibration.reportedTokens(b, "ai") == 7)

        #expect(AIIncarnation.of(a, "ai") == 0)
        #expect(AIIncarnation.bump(a, "ai") == 1)
        #expect(AIIncarnation.bump(a, "ai") == 2)
        #expect(AIIncarnation.of(b, "ai") == 0)
        AIIncarnation.set(b, "ai", 7)
        #expect(AIIncarnation.of(a, "ai") == 2 && AIIncarnation.of(b, "ai") == 7)

        #expect(AIRunStates.of(a, "ai") === AIRunStates.of(a, "ai"))
        #expect(AIRunStates.of(a, "ai") !== AIRunStates.of(b, "ai"))
        #expect(AIRunStates.of(a, "ai") !== AIRunStates.of(a, "other"))
        #expect(AIMemory.of(a) === AIMemory.of(a))
        #expect(AIMemory.of(a) !== AIMemory.of(b))

        #expect(AISilentStreaks.bump(a, "ai", "c") == 1)
        #expect(AISilentStreaks.bump(a, "ai", "c") == 2)
        #expect(AISilentStreaks.bump(a, "ai", "other") == 1, "a streak is per (agent, client)")
        #expect(AISilentStreaks.bump(b, "ai", "c") == 1)
        AISilentStreaks.reset(a, "ai", "c")
        #expect(AISilentStreaks.bump(a, "ai", "c") == 1)
        AISilentStreaks.setCap(a, 3)
        #expect(AISilentStreaks.cap(a) == 3)
        #expect(AISilentStreaks.cap(b) == AIConstants.MAX_SILENT_TURNS)
    }

    /// A kernel's shutdown drops every kernel-keyed row it owned; another live
    /// kernel's rows stay.
    @Test func teardownEmptiesTheKernelsRowsAndOnlyThose() async {
        AISideTables.installTeardownHook()
        let dying = bareKernel()
        let living = bareKernel()
        for k in [dying, living] {
            AICalibration.record(k, "ai", reported: 400, estimate: 100)
            AIIncarnation.set(k, "ai", 5)
            _ = AISilentStreaks.bump(k, "ai", "c")
            AISilentStreaks.setCap(k, 2)
        }
        let run = AIRunStates.of(dying, "ai")
        let mem = AIMemory.of(dying)
        let liveRun = AIRunStates.of(living, "ai")
        let liveMem = AIMemory.of(living)

        await dying.shutdown()

        #expect(AICalibration.calibration(dying, "ai") == 1.0)
        #expect(AICalibration.reportedTokens(dying, "ai") == nil)
        #expect(AIIncarnation.of(dying, "ai") == 0)
        #expect(AIRunStates.of(dying, "ai") !== run, "the run state was not dropped")
        #expect(AIMemory.of(dying) !== mem, "the memory state was not dropped")
        #expect(AISilentStreaks.bump(dying, "ai", "c") == 1)
        #expect(AISilentStreaks.cap(dying) == AIConstants.MAX_SILENT_TURNS)

        #expect(AICalibration.calibration(living, "ai") == 4.0)
        #expect(AICalibration.reportedTokens(living, "ai") == 400)
        #expect(AIIncarnation.of(living, "ai") == 5)
        #expect(AIRunStates.of(living, "ai") === liveRun)
        #expect(AIMemory.of(living) === liveMem)
        #expect(AISilentStreaks.bump(living, "ai", "c") == 2)
        #expect(AISilentStreaks.cap(living) == 2)
        await living.shutdown()
    }

    /// The strip table's test seams: `set` seeds, `has` sees it, `forget` drops
    /// it.
    @Test func theStripSeams() {
        let id = "strip-seam-\(UUID().uuidString.prefix(8))"
        #expect(!AIStripStrategy.has(id))
        AIStripStrategy.set(id, ["a:00000001"])
        #expect(AIStripStrategy.has(id))
        AIStripStrategy.forget(id)
        #expect(!AIStripStrategy.has(id))
    }

    /// One generation at a time per agent: queued acquirers are served in
    /// ARRIVAL order, and `busy` is true exactly while the lock is held.
    @Test func theFifoLockServesInArrivalOrder() async {
        let lock = AIFifoLock()
        #expect(await lock.busy() == false)
        await lock.acquire()
        #expect(await lock.busy() == true)
        let order = Order()
        var tasks: [Task<Void, Never>] = []
        for i in 1...5 {
            tasks.append(
                Task {
                    await lock.acquire()
                    order.add(i)
                    await lock.release()
                })
            try? await Task.sleep(nanoseconds: 20_000_000)
        }
        #expect(order.value.isEmpty)
        await lock.release()
        for t in tasks { await t.value }
        #expect(order.value == [1, 2, 3, 4, 5])
        #expect(await lock.busy() == false)
    }
}
