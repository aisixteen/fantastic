// A batch EXECUTES in parallel (owner, 2026-10-01: "the idea was parallel
// executing — it needs to hold"). Since 342b372f a batch STARTS its items in input
// order — item N+1 is handed to its target only once item N has fanned out — so
// this pins the other half: the items still RUN at the same time. Observed, not
// timed: item A waits until item B has STARTED; a batch that serialized execution
// could never get B going while A runs, and A records that it gave up.

import FantasticJSON
import Foundation
import Testing

@testable import FantasticKernel

/// What the probe handlers saw: whether B started, and whether A saw it.
final class OverlapBox: @unchecked Sendable {
    private let lock = NSLock()
    private var bStarted = false
    private(set) var aSawB: Bool? = nil

    func markBStarted() { lock.withLock { bStarted = true } }
    func didBStart() -> Bool { lock.withLock { bStarted } }
    func recordA(_ saw: Bool) { lock.withLock { aSawB = saw } }
    var result: Bool? { lock.withLock { aSawB } }
}

/// `a`: hold until `b` has started (bounded, 3 s), then report whether it did.
/// `b`: mark started and return.
struct OverlapBundle: AgentBundle {
    let name = "overlap"
    let box: OverlapBox
    func handle(agentId: AgentId, payload: JSON, kernel: Kernel) async throws -> JSON? {
        switch payload["type"].asString {
        case "a":
            let deadline = Date().addingTimeInterval(3)
            while !box.didBStart() && Date() < deadline {
                try? await Task.sleep(nanoseconds: 5_000_000)
            }
            let saw = box.didBStart()
            box.recordA(saw)
            return .object(["overlapped": .bool(saw)])
        case "b":
            box.markBStarted()
            return .object(["ok": .bool(true)])
        default:
            return .object(["error": .string("unknown verb")])
        }
    }
}

/// The order traffic REACHED its targets — the `send` state events, which the
/// dispatch prologue publishes as it fans out.
final class ArrivalLog: @unchecked Sendable {
    private let lock = NSLock()
    private var verbs: [String] = []
    func append(_ v: String) { lock.withLock { verbs.append(v) } }
    var all: [String] { lock.withLock { verbs } }
}

@Suite("Batch executes in parallel")
struct BatchOverlapTests {
    private func kernel(_ box: OverlapBox, agents: [String]) -> Kernel {
        let registry = BundleRegistry()
        registry.register("overlap.tools", OverlapBundle(box: box))
        let k = Kernel(storage: .inMemory, bundles: registry)
        let root = Agent(id: "kernel_state", handlerModule: nil, parentId: nil)
        k.register(root)
        k.setRoot(root)
        for id in agents {
            k.register(Agent(id: AgentId(id), handlerModule: "overlap.tools", parentId: AgentId("kernel_state")))
        }
        return k
    }

    private func run(_ k: Kernel, _ box: OverlapBox, to targets: (String, String)) async
        -> ([Outcome], [String])
    {
        let log = ArrivalLog()
        let token = k.subscribe { evt, _ in
            if evt["kind"].asString == "send", let v = evt["payload"]["type"].asString,
                v == "a" || v == "b"
            {
                log.append(v)
            }
        }
        defer { k.unsubscribe(token) }
        let out = await k.send([
            Item(AgentId(targets.0), ["type": "a"]),
            Item(AgentId(targets.1), ["type": "b"]),
        ])
        return (out, log.all)
    }

    @Test func twoItemsToOneAgentRunAtTheSameTime() async {
        let box = OverlapBox()
        let k = kernel(box, agents: ["solo"])
        let (out, arrivals) = await run(k, box, to: ("solo", "solo"))
        #expect(box.result == true, "A never saw B start — the batch ran its items one after the other")
        #expect(out[0].result["overlapped"].asBool == true, "\(out[0].result)")
        #expect(out[1].status == .completed)
        #expect(arrivals == ["a", "b"], "items reach their target in input order: \(arrivals)")
    }

    @Test func twoItemsToTwoAgentsRunAtTheSameTime() async {
        let box = OverlapBox()
        let k = kernel(box, agents: ["left", "right"])
        let (out, arrivals) = await run(k, box, to: ("left", "right"))
        #expect(box.result == true, "A never saw B start — the batch ran its items one after the other")
        #expect(out[0].result["overlapped"].asBool == true, "\(out[0].result)")
        #expect(out[1].status == .completed)
        #expect(arrivals == ["a", "b"], "items reach their targets in input order: \(arrivals)")
    }
}
