// The fold/strip/view fixtures — python `test_folds.py`'s `_K`, `_id`, `_row`,
// `_chain`, `_fold`, `_interleave` and `_fake_summary`, shared by `FoldsTests`,
// `ViewRecallTests` and `ContextSeamTests` exactly as python's three files share
// them.

import FantasticJSON
import FantasticKernel
import Foundation
import OrderedCollections
import Testing

@testable import FantasticAICore

/// python `BIG` in test_folds / test_view_recall.
let FOLD_BIG = String(repeating: "x", count: 3000)

/// python `_fake_summary`.
@Sendable func fakeSummary(_ msgs: [JSON]) async throws -> String {
    "TITLE of the fold\n\nBody of the fold: what happened."
}

/// The store a fixture writes to, whose mints draw from the SAME allocator as
/// the fixture tags — one counter, like the kernel's — so a row the product
/// mints (a view, a body, a fold) never lands on an id a later fixture row was
/// going to carry. python `_K` + `_id`.
final class FoldFixture: @unchecked Sendable {
    let kernel: Kernel
    let agent: AgentId
    private let lock = NSLock()
    private var tags: [String: Int64] = [:]

    /// A bare kernel is a row store; the boot helpers hand a real one in.
    init(kernel: Kernel = Kernel(), agent: String = "ai") {
        self.kernel = kernel
        self.agent = AgentId(agent)
    }

    /// A fixture TAG ("user:goal", "ai:a0r") as the int id the store holds —
    /// the same tag is the same id for the fixture's lifetime, minted from the
    /// store's own counter on first sight.
    func id(_ tag: String) -> Int64 {
        lock.lock()
        defer { lock.unlock() }
        if let n = tags[tag] { return n }
        let n = kernel.rows.mintId()
        tags[tag] = n
        return n
    }

    func idJSON(_ tag: String?) -> JSON {
        tag.map { .integer(id($0)) } ?? .null
    }

    /// python `_row(role, content, id_, parent=None, **extra)`.
    func row(
        _ role: String, _ content: String, _ id: String?, _ parent: String? = nil,
        extra: OrderedDictionary<String, JSON> = [:]
    ) -> JSON {
        row(role, content, id, parentJSON: idJSON(parent), extra: extra)
    }

    /// `_row(...)` whose parent is an int the caller already holds.
    func row(_ role: String, _ content: String, _ id: String?, parentId: Int64?) -> JSON {
        row(role, content, id, parentJSON: parentId.map(JSON.integer) ?? .null)
    }

    private func row(
        _ role: String, _ content: String, _ id: String?, parentJSON: JSON,
        extra: OrderedDictionary<String, JSON> = [:]
    ) -> JSON {
        var obj: OrderedDictionary<String, JSON> = [
            "role": .string(role), "content": .string(content),
            "id": idJSON(id), "parent": parentJSON,
        ]
        for (k, v) in extra { obj[k] = v }
        return .object(obj)
    }

    /// root user msg → (assistant call → reply) × n, optionally ending on an
    /// unanswered call. python `_chain`.
    func chain(_ tag: String, _ nCalls: Int, big: Bool = true, openEnd: Bool = false) -> [JSON] {
        var rows = [row("user", "task \(tag)", "user:\(tag)")]
        var prev = "user:\(tag)"
        for i in 0..<nCalls {
            let a = "ai:\(tag)\(i)"
            rows.append(
                row(
                    "assistant",
                    "read \(tag)\(i) <call name=\"send\"><target_id>notes</target_id><title>read \(tag)\(i)</title><type>read</type></call>",
                    a, prev))
            let r = "ai:\(tag)\(i)r"
            rows.append(
                row(
                    "user",
                    "[reply to notes:\(tag)\(i) · notes read · \"read \(tag)\(i)\" · ↑\(a)]\n\(big ? FOLD_BIG : "ok")",
                    r, a))
            prev = r
        }
        if openEnd {
            rows.append(
                row(
                    "assistant",
                    "one more <call name=\"send\"><target_id>notes</target_id><type>read</type></call>",
                    "ai:\(tag)open", prev))
        }
        return rows
    }

    /// `make_fold` for tests: the store mints in the product; here a fixture id
    /// unless the test names one. python `_fold`.
    func fold(
        _ refs: [Int64], _ title: String, _ body: String, _ degraded: Bool,
        parent: Int64? = nil, id: Int64? = nil
    ) -> JSON {
        AIFolds.makeFold(
            refs: refs, title: title, body: body, degraded: degraded,
            id: id ?? kernel.rows.mintId(), parent: parent)
    }

    /// The ids of a chain's rows (every row of a fixture chain carries one).
    func ids(_ rows: [JSON]) -> [Int64] { rows.compactMap(rowId) }

    /// python `_seed` / `_save_history`.
    func seed(_ rows: [JSON]) { appendRows(agent, kernel: kernel, rows) }

    /// python `k.rows_of()` — every row for the agent, bookkeeping included.
    func rowsOf() -> [JSON] { kernel.rows.forAgent(agent).map(JSON.object) }

    /// python `k.folds_of()`.
    func foldsOf() -> [JSON] { rowsOf().filter(isFold) }

    /// python `_ctx(bud, kernel, summarize)` of test_folds: recent_n 4,
    /// self_id the fixture agent, client_id "c" (so `fold` PERSISTS).
    func ctx(
        _ budget: Int64, summarize: (@Sendable ([JSON]) async throws -> String)? = fakeSummary,
        recentN: Int = 4, clientId: String? = "c"
    ) -> ProjectionCtx {
        ProjectionCtx(
            budget: budget, recentN: recentN, summarize: summarize, selfId: agent,
            kernel: kernel, clientId: clientId)
    }
}

/// python `_interleave` — `random.Random(7)` picks a chain per step. The exact
/// sequence is not the claim; determinism is, so a fixed LCG stands in.
func interleave(_ chains: [[JSON]]) -> [JSON] {
    var ptr = [Int](repeating: 0, count: chains.count)
    var state: UInt64 = 7
    var out: [JSON] = []
    while true {
        let live = chains.indices.filter { ptr[$0] < chains[$0].count }
        if live.isEmpty { break }
        state = state &* 6_364_136_223_846_793_005 &+ 1_442_695_040_888_963_407
        let i = live[Int((state >> 33) % UInt64(live.count))]
        out.append(chains[i][ptr[i]])
        ptr[i] += 1
    }
    return out
}

/// A body row with no id — python `_turn`.
func plainTurn(_ role: String, _ content: String) -> JSON {
    .object(["role": .string(role), "content": .string(content)])
}

/// A provider whose answer is COMPUTED from the request — python's inline `_P`
/// classes, which read the prompt they were handed and answer accordingly.
final class ComputedProvider: AIProvider, @unchecked Sendable {
    let model = "computed"
    private let answer: @Sendable ([JSON]) -> String
    init(_ answer: @escaping @Sendable ([JSON]) -> String) { self.answer = answer }
    func chat(messages: [JSON]) -> AsyncThrowingStream<AIChunk, Error> {
        let text = answer(messages)
        return AsyncThrowingStream { c in
            c.yield(.token(text))
            c.finish()
        }
    }
}

/// The private reaction cursor, read through the backend's own lock.
/// SYNCHRONOUS on purpose: `NSLock.lock()` is unavailable from an async context.
func markOf(_ backend: AIBackend, _ key: String) -> (Int, String)? {
    backend.projectionLock.lock()
    defer { backend.projectionLock.unlock() }
    return backend.compactionMark[key]
}

/// The `context` events an agent's inbox carried.
func contextEvents(_ sink: EventSink) -> [JSON] {
    sink.snapshot().filter { $0["type"].asString == "context" }
}
