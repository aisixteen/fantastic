// #34–#37 — the execution ledger, as a CALLER sees it through `send`.
//
// In-process invariants: a named id runs once and only once, a concurrent
// re-send JOINS rather than re-running, a retry budget re-dispatches under the
// same handle, and a spent budget parks the row as a dead letter that is never
// silently re-armed.

import FantasticJSON
import Foundation
import OrderedCollections
import Testing

@testable import FantasticKernel

/// A handler THROW — the kernel settles it as `failed` with `raised: true`,
/// which is what the retry engine re-spends its budget on.
private struct FlakyMiss: Error { let n: Int }

private final class Runs: @unchecked Sendable {
    private let lock = NSLock()
    private var n = 0
    private var failUntil = 0
    func setFailUntil(_ v: Int) {
        lock.lock(); defer { lock.unlock() }
        failUntil = v
    }
    func bump() throws -> Int {
        lock.lock(); defer { lock.unlock() }
        n += 1
        if n < failUntil { throw FlakyMiss(n: n) }
        return n
    }
    var count: Int {
        lock.lock(); defer { lock.unlock() }
        return n
    }
}

private struct CountBundle: AgentBundle {
    let name = "counting"
    let runs: Runs
    func handle(agentId: AgentId, payload: JSON, kernel: Kernel) async throws -> JSON? {
        switch payload["type"].asString ?? "" {
        case "boot", "shutdown": return nil
        case "work": return .object(["run": .integer(Int64(try runs.bump()))])
        case "slow":
            try? await Task.sleep(nanoseconds: 30_000_000_000)
            return .object(["finished": .bool(true)])
        default: return .object(["error": .string("unknown verb")])
        }
    }
}

@Suite("Execution ledger: once-only, join, retry, dead letter")
struct LedgerTests {
    private func makeKernel(_ runs: Runs) async -> Kernel {
        let reg = BundleRegistry()
        reg.register("counting.tools", CountBundle(runs: runs))
        let k = Kernel(storage: .inMemory, bundles: reg)
        let root = Agent(id: "core", handlerModule: nil, parentId: nil)
        k.register(root)
        k.setRoot(root)
        _ = await k.send([
            Item(
                AgentId("kernel"),
                .object([
                    "type": .string("create_agent"), "parent_id": .string("kernel"),
                    "handler_module": .string("counting.tools"), "id": .string("desk"),
                ]))
        ])
        return k
    }

    @Test func aNamedIdRunsTheHandlerExactlyOnce() async {
        // C9/C11: the SECOND send of a settled id returns the recorded outcome
        // and the handler does not run. This is the whole dedup contract.
        let runs = Runs()
        let k = await makeKernel(runs)

        let first = await k.send([
            Item(AgentId("desk"), .object(["type": .string("work")]), id: "job-1")
        ])[0]
        #expect(first.status == .completed)
        #expect(first.result["run"].asInt == 1)
        #expect(runs.count == 1)

        for _ in 0..<4 {
            let again = await k.send([
                Item(AgentId("desk"), .object(["type": .string("work")]), id: "job-1")
            ])[0]
            #expect(again.result["run"].asInt == 1, "the recorded outcome must return")
        }
        #expect(runs.count == 1, "the handler ran more than once")
    }

    @Test func concurrentSendsOfOneIdRunTheBodyOnce() async {
        // C10: once-only under concurrency — what makes a double-click safe.
        let runs = Runs()
        let k = await makeKernel(runs)
        async let a = k.send([
            Item(AgentId("desk"), .object(["type": .string("work")]), id: "click-7")
        ])
        async let b = k.send([
            Item(AgentId("desk"), .object(["type": .string("work")]), id: "click-7")
        ])
        let (l, r) = await (a, b)
        #expect(runs.count == 1, "a race ran the body twice")
        #expect(l[0].result == r[0].result, "both callers see one outcome")
    }

    @Test func anUnnamedExecutionIsLedgeredUnderItsPid() async {
        // ONE ID SPACE: every execution is a row, so any reply is re-fetchable
        // by the pid `ps`/`cancel` already use.
        let runs = Runs()
        let k = await makeKernel(runs)
        let out = await k.send([Item(AgentId("desk"), .object(["type": .string("work")]))])[0]
        let pid = try! #require(out.pid)

        let rows = await k.send([
            Item(AgentId("kernel"), .object(["type": .string("ledger")]))
        ])[0].result["executions"].asArray ?? []
        let row = rows.first { $0["id"].asInt == pid.value }
        #expect(row?["named"].asBool == false)
        #expect(row?["status"].asString == "completed")

        // NO RE-FETCH BY PID. Python's contract: a caller supplies a NAME, never
        // an id — "a name it has seen JOINS that execution, a name it has not
        // opens a new one". An unnamed pid is not a name; its outcome is read
        // off the `ledger`, above. (The old assertion here was a swift-only
        // shortcut, deleted 2026-09-12.)
        #expect(runs.count == 1)
    }

    @Test func aRetryBudgetReDispatchesUnderTheSameId() async {
        // Only the FINAL outcome settles — a joiner never sees an attempt that
        // was retried away. `attempts` reports what the work actually cost.
        let runs = Runs()
        runs.setFailUntil(3)
        let k = await makeKernel(runs)
        let out = await k.send([
            Item(
                AgentId("desk"), .object(["type": .string("work")]),
                id: "flaky-1", retries: 5)
        ])[0]
        #expect(out.status == .completed, "\(out.result)")
        #expect(out.attempts == 3, "attempts must report the real cost")
        #expect(runs.count == 3)
    }

    @Test func aSpentBudgetParksADeadLetterThatNeverReArms() async {
        // C12: failed work must neither vanish nor loop. It parks WITH its
        // outcome, queryable; a retry is an explicit NEW id.
        let runs = Runs()
        runs.setFailUntil(99)
        let k = await makeKernel(runs)
        let out = await k.send([
            Item(AgentId("desk"), .object(["type": .string("work")]), id: "dead-1", retries: 1)
        ])[0]
        #expect(out.status == .failed)
        #expect(out.attempts == 2, "one attempt plus one retry")
        #expect(runs.count == 2)

        // INSPECTABLE — terminal non-completed rows are the dead-letter queue.
        let rows = await k.send([
            Item(AgentId("kernel"), .object(["type": .string("ledger")]))
        ])[0].result["executions"].asArray ?? []
        // BY NAME: a caller-named row carries `name`; its `id` is the kernel's int.
        let dead = rows.first { $0["name"].asString == "dead-1" }
        #expect(dead?["status"].asString == "failed")
        #expect(dead?["attempts"].asInt == 2)

        // Re-sending the same id — even with a bigger budget — returns the
        // recorded failure. Nothing is retried silently.
        let again = await k.send([
            Item(AgentId("desk"), .object(["type": .string("work")]), id: "dead-1", retries: 9)
        ])[0]
        #expect(again.status == .failed)
        #expect(runs.count == 2, "a parked dead letter re-armed")
    }

    @Test func anIdIsBoundToItsFirstTarget() async {
        let runs = Runs()
        let k = await makeKernel(runs)
        _ = await k.send([
            Item(AgentId("desk"), .object(["type": .string("work")]), id: "bound-1")
        ])
        let reaimed = await k.send([
            Item(AgentId("core"), .object(["type": .string("reflect")]), id: "bound-1")
        ])[0]
        #expect(reaimed.status == .failed)
        #expect(reaimed.result["reason"].asString == "bad_arg")
        #expect(reaimed.result["bound_to"].asString == "desk")
    }

    @Test func theLedgerVerbIsPagedLikeEveryOtherListing() async {
        let runs = Runs()
        let k = await makeKernel(runs)
        for i in 0..<4 {
            _ = await k.send([
                Item(AgentId("desk"), .object(["type": .string("work")]), id: "p\(i)")
            ])
        }
        let p = await k.send([
            Item(
                AgentId("kernel"),
                .object(["type": .string("ledger"), "limit": .integer(2)]))
        ])[0].result
        #expect(p["executions"].asArray?.count == 2)
        #expect(p["next"].asString != nil, "a further page must be named")
    }
}

extension LedgerTests {
    /// FAILURE IS A STATUS. A handler that RAN and said no — here an unknown
    /// verb, but equally a missing path or a patch that did not match — settles
    /// `errored`, not `completed`.
    ///
    /// It used to settle `completed`, on the rule that the kernel does not
    /// introspect domain replies, while the durable row already carried the
    /// error copied off the result beside `status: completed`. One row said both
    /// "fine" and "failed". Python and rust each had a test that moved when this
    /// changed; swift had none, so the old behaviour was unpinned here.
    @Test func aHandlerThatSaysNoSettlesErrored() async {
        let runs = Runs()
        let k = await makeKernel(runs)

        let out = await k.send([
            Item(AgentId("desk"), .object(["type": .string("nope_unknown")]))
        ])[0]
        #expect(out.status == .errored, "\(out.status) — \(out.result.serialize())")
        #expect(out.result["error"].asString == "unknown verb")

        // AN ERRORED EXECUTION IS STILL AN EXECUTION: the ledger lists what work
        // BECAME, and a refusal is an outcome like any other.
        let led = await k.send([
            Item(AgentId("kernel"), .object(["type": .string("ledger")]))
        ])[0].result
        // BY PID — the agent also has a child-boot row under the same target.
        let pid = out.pid?.value
        let row = (led["executions"].asArray ?? []).first { $0["id"].asInt == pid }
        #expect(row != nil, "the errored execution vanished: \(led.serialize())")
        #expect(row?["status"].asString == "errored", "\(row?.serialize() ?? "-")")
    }

    /// A THROW is `failed`, not `errored` — the two are different facts and the
    /// retry engine only re-spends the transport class.
    @Test func aThrowStaysFailedAndErroredNeverRetries() async {
        let runs = Runs()
        runs.setFailUntil(99)  // always throws
        let k = await makeKernel(runs)

        let thrown = await k.send([
            Item(AgentId("desk"), .object(["type": .string("work")]), retries: 2)
        ])[0]
        #expect(thrown.status == .failed, "\(thrown.status)")
        #expect(thrown.attempts == 3, "a throw is retried: \(thrown.attempts)")

        let said = await k.send([
            Item(AgentId("desk"), .object(["type": .string("nope")]), retries: 2)
        ])[0]
        #expect(said.status == .errored)
        #expect(said.attempts == 1, "a semantic no is never re-sent: \(said.attempts)")
    }
}
