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
        lock.lock()
        defer { lock.unlock() }
        failUntil = v
    }
    func bump() throws -> Int {
        lock.lock()
        defer { lock.unlock() }
        n += 1
        if n < failUntil { throw FlakyMiss(n: n) }
        return n
    }
    var count: Int {
        lock.lock()
        defer { lock.unlock() }
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
        let root = Agent(id: "kernel_state", handlerModule: nil, parentId: nil)
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

        let rows =
            await k.send([
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
        let rows =
            await k.send([
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

    /// C80: `ExecutionLedger.setCap` sets the ring's cap AND immediately
    /// re-runs eviction against it — RETROACTIVE, unlike python's plain
    /// `ledger_cap` attribute set (only bites on the NEXT insert; harmless
    /// since the CLI always sets a cap before any execution exists). Same
    /// ring rule either way: oldest SETTLED unnamed rows go first, a named
    /// row (a dedup contract) outlives them, nothing in flight is evicted.
    @Test func setCapEvictsLikeTheConstructorCap() async {
        let runs = Runs()
        let k = await makeKernel(runs)
        // A named row FIRST — the OLDEST entry, so surviving the cap proves
        // named rows are evicted LAST, not merely "still young enough".
        let named = await k.send([
            Item(AgentId("desk"), .object(["type": .string("work")]), id: "keepme")
        ])[0]
        var pids: [Int64] = []
        for _ in 0..<4 {
            let o = await k.send([Item(AgentId("desk"), .object(["type": .string("work")]))])[0]
            pids.append(try! #require(o.pid).value)
        }
        // Five settled rows now, all under the ledger's (huge) default cap —
        // nothing evicted yet.
        #expect(k.ledger.has(pids[0]))

        // Retroactively shrink the cap on an ALREADY over-cap ledger.
        k.ledger.setCap(3)

        #expect(!k.ledger.has(pids[0]), "the oldest unnamed row is evicted")
        #expect(!k.ledger.has(pids[1]), "the second-oldest unnamed row is evicted")
        #expect(k.ledger.has(pids[3]), "the newest unnamed row survives")

        // The named row survives the unnamed churn and still dedups.
        let again = await k.send([
            Item(AgentId("desk"), .object(["type": .string("work")]), id: "keepme")
        ])[0]
        #expect(again.result == named.result)
        #expect(runs.count == 5, "the join must not have re-run the handler")
    }

    /// C31 r2 correction: BOTH the text and the extras render the CALLER's
    /// RAW id (`pyRepr` for a string), never the minted exec_id; the target
    /// names via `pyRepr` too; there is no `bound_to`.
    @Test func anIdIsBoundToItsFirstTarget() async {
        let runs = Runs()
        let k = await makeKernel(runs)
        _ = await k.send([
            Item(AgentId("desk"), .object(["type": .string("work")]), id: "bound-1")
        ])
        let reaimed = await k.send([
            Item(AgentId("kernel_state"), .object(["type": .string("reflect")]), id: "bound-1")
        ])[0]
        #expect(reaimed.status == .failed)
        #expect(reaimed.result["reason"].asString == "bad_arg")
        #expect(
            reaimed.result["error"].asString
                == "send: id 'bound-1' already names an execution on 'desk', not 'kernel_state' "
                + "— an id is bound to its first execution; a retry aimed elsewhere needs a "
                + "new id",
            "\(reaimed.result)")
        #expect(reaimed.result["bound_to"] == nil)
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

    /// C16: python's exact key order — `{id, target, attempts, retries,
    /// named, name?, status, outcome?}`. `status`/`outcome` land AFTER
    /// `named`/`name`, not right after `target`.
    @Test func ledgerRowKeyOrderMatchesPython() async {
        let runs = Runs()
        let k = await makeKernel(runs)
        _ = await k.send([
            Item(AgentId("desk"), .object(["type": .string("work")]), id: "job-order")
        ])
        let unnamed = await k.send([Item(AgentId("desk"), .object(["type": .string("work")]))])[0]
        let r = await k.send([Item(AgentId("kernel"), .object(["type": .string("ledger")]))])[0]
            .result
        let rows = r["executions"].asArray ?? []
        let named = rows.first { $0["name"].asString == "job-order" }
        #expect(
            named?.asObject.map { Array($0.keys) }
                == ["id", "target", "attempts", "retries", "named", "name", "status", "outcome"],
            "\(String(describing: named))")
        let bare = rows.first { $0["id"].asInt == unnamed.pid?.value }
        #expect(
            bare?.asObject.map { Array($0.keys) }
                == ["id", "target", "attempts", "retries", "named", "status", "outcome"],
            "\(String(describing: bare))")
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

    /// C46 — the sidecar is python's v2 document, and every terminal status
    /// comes back: an `errored` row read as anything else would re-arm the side
    /// effect the ledger exists to prevent.
    @Test func testSidecarIsPythonV2Bytes() async throws {
        let ledger = ExecutionLedger()
        for (id, status) in [
            (7 as Int64, Status.completed), (9, .errored), (11, .failed), (13, .cancelled),
        ] {
            _ = ledger.open(
                id, target: AgentId("desk"), payload: .object(["type": .string("work")]),
                retries: 0, name: id == 7 ? "job-1" : nil, origin: AgentId("caller"),
                callerId: id == 7 ? .string("job-1") : .integer(id))
            _ = ledger.settle(
                id, Outcome(pid: TaskId(id), status: status, result: .object([:]), attempts: 1))
        }
        let snap = ledger.settledSnapshot()
        #expect(snap["v"].asInt == 2)
        let rows = try #require(snap["rows"].asArray)
        #expect(rows.count == 4)
        // PYTHON'S KEY ORDER, and NO `verb`.
        #expect(
            rows[0].asObject.map { Array($0.keys) }
                == ["id", "target", "retries", "attempts", "named", "name", "origin", "outcome"],
            "\(rows[0])")
        #expect(rows[0]["id"].asInt == 7 && rows[0]["name"].asString == "job-1")
        #expect(rows[1]["name"] == .null && rows[1]["named"].asBool == false)

        // A FRESH ledger restores every one of them, `errored` included…
        let fresh = ExecutionLedger()
        #expect(fresh.rehydrate(snap) == 4)
        #expect(fresh.settledOutcome(9)?.status == .errored)
        // …and the id counter is raised above the highest row.
        #expect(fresh.highestId() == 13)
    }

    /// C46 / A11.5 — a settled outcome that carried RAW BYTES is RAM-ONLY.
    /// Python's `_ledger_dump` drops any row `json.dumps` cannot encode, and
    /// "bytes in a read_stream result" is the case it names; swift carries the
    /// bytes on the outcome's binary channel instead, so the JSON always
    /// encoded and the row persisted — measured as `{path, offset, size,
    /// next_offset, eof}` rows in `ledger.json` that python has none of.
    @Test func testSidecarDropsOutcomesThatCarriedBytes() async throws {
        let ledger = ExecutionLedger()
        for id in [21 as Int64, 22] {
            _ = ledger.open(
                id, target: AgentId("store"),
                payload: .object(["type": .string("read_stream")]), retries: 0, name: nil,
                origin: AgentId("rest"), callerId: .integer(id))
        }
        let envelope: JSON = .object([
            "path": .string("a.txt"), "offset": .integer(0), "size": .integer(3),
            "next_offset": .integer(3), "eof": .bool(true),
        ])
        _ = ledger.settle(21, Outcome(pid: TaskId(21), status: .completed, result: envelope))
        _ = ledger.settle(
            22,
            Outcome(
                pid: TaskId(22), status: .completed, result: envelope,
                body: Data("abc".utf8)))
        let rows = try #require(ledger.settledSnapshot()["rows"].asArray)
        #expect(rows.map { $0["id"].asInt } == [21], "\(rows)")
        // The row is still LIVE in RAM — dedup holds for this process.
        #expect(ledger.settledOutcome(22)?.status == .completed)
    }

    /// RP-08 — each ledger flush pass is a REGISTERED kernel task on the root,
    /// verb `ledger_flush`, so `ps` sees it, `cancel` reaches it and the message
    /// pump can recognise the kernel's own persistence traffic. A bare `Task`
    /// was invisible to all three.
    @Test func testFlushRunsAsLedgerFlushTask() async {
        let runs = Runs()
        let k = await makeKernel(runs)
        await k.installLedgerPersistence()
        _ = await k.send([
            Item(AgentId("desk"), .object(["type": .string("work")]), id: "job-1")
        ])[0]
        try? await Task.sleep(nanoseconds: 100_000_000)
        let flushRows = k.rows.forAgent(k.root!.id).filter {
            $0["kind"]?.asString == "task" && $0["verb"]?.asString == "ledger_flush"
        }
        #expect(!flushRows.isEmpty, "the flush pass minted no `ledger_flush` task row")
        #expect(flushRows.first?["parent"] != nil, "it is parented to the settling dispatch")
    }
}
