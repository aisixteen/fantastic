// The fold/strip/view fixtures — python `test_folds.py`'s `_K`, `_id`, `_row`,
// `_chain`, `_fold`, `_render`, `_interleave` and `_fake_summary`, shared by `FoldsTests`,
// `ViewRecallTests` and `ContextSeamTests` exactly as python's three files share
// them.

import FantasticJSON
import FantasticKernel
import Foundation
import OrderedCollections
import Testing

@testable import FantasticAICore
@testable import FantasticRamStore

/// A kernel wired the way an AI agent needs one — python's `FakeCtx`: a root
/// carrying a guid, a `memory` ram_store, and the agent's own record naming it
/// (`memory_store_id`), the two gates granted both ways.
func wiredKernel(
    agent: String = "ai", meta: OrderedDictionary<String, JSON> = [:]
) -> Kernel {
    let registry = BundleRegistry()
    registry.register("ram_store.tools", RamStoreBundle())
    let k = Kernel(storage: .inMemory, bundles: registry)
    let root = Agent(
        id: AgentId("kernel_state"), handlerModule: nil, parentId: nil,
        meta: ["guid": .string(MOCK_GUID)])
    k.register(root)
    k.setRoot(root)
    k.composeEphemeralChild(
        id: AgentId("memory"), handlerModule: "ram_store.tools", parent: root,
        meta: [
            "callable_by": .object([agent: .string("*")]),
        ])
    var m: OrderedDictionary<String, JSON> = [
        "memory_store_id": .string("memory"),
        "can_call": .object(["memory": .string("*")]),
    ]
    for (kk, v) in meta { m[kk] = v }
    k.composeEphemeralChild(
        id: AgentId(agent), handlerModule: "fixture_ai.tools", parent: root, meta: m)
    return k
}

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

    /// A wired kernel (the agent + its memory store); the boot helpers hand a
    /// real one in.
    init(kernel: Kernel? = nil, agent: String = "ai") {
        self.kernel = kernel ?? wiredKernel(agent: agent)
        self.agent = AgentId(agent)
    }

    /// A fixture TAG ("user:goal", "ai:a0r") as the int id the store holds —
    /// the same tag is the same id for the fixture's lifetime, minted from the
    /// store's own counter on first sight.
    func id(_ tag: String) -> Int64 {
        lock.lock()
        defer { lock.unlock() }
        if let n = tags[tag] { return n }
        let n = kernel.mintId()
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

    /// The anchor of every fold this fixture made, by fold id.
    private var foldAnchors: [Int64: Int64] = [:]

    /// `make_fold` for tests: the store mints in the product; here a fixture id
    /// unless the test names one. The anchor is what the product writes — the
    /// smallest leaf id, a member fold made here contributing its own anchor; a
    /// fold with no refs names its anchor. python `_fold`.
    func fold(
        _ refs: [Int64], _ title: String, _ body: String, _ degraded: Bool,
        parent: Int64? = nil, id: Int64? = nil, anchor: Int64? = nil
    ) -> JSON {
        lock.lock()
        let derived = refs.map { foldAnchors[$0] ?? $0 }.min()
        lock.unlock()
        guard let a = anchor ?? derived else {
            preconditionFailure("fold: a fold with no refs names its anchor")
        }
        let fd = AIFolds.makeFold(
            refs: refs, title: title, body: body, degraded: degraded,
            id: id ?? kernel.mintId(), anchor: a, parent: parent)
        if let fid = rowId(fd) {
            lock.lock()
            foldAnchors[fid] = a
            lock.unlock()
        }
        return fd
    }

    /// The anchor this fixture gave the fold `id` (nil: not a fixture fold).
    func anchorOf(_ id: Int64) -> Int64? {
        lock.lock()
        defer { lock.unlock() }
        return foldAnchors[id]
    }

    /// python `_render`: `render` over rows that are the whole head — anchors
    /// from the same rows.
    func render(_ rows: [JSON]) -> [JSON] {
        AIFolds.render(rows, AIFolds.anchorsOf(rows))
    }

    /// python `_interleave`: the chains merged in a fixed pseudo-random order,
    /// then — ARRIVAL ORDER IS ID ORDER — every row minted again, in place, in
    /// the order it arrives (its tag follows), as the kernel's counter would have
    /// numbered it. `chains` is rewritten with the new ids, so a test that reads
    /// a chain afterwards sees the ids the merged rows carry.
    func interleave(_ chains: inout [[JSON]]) -> [JSON] {
        let order = interleaveOrder(chains.map(\.count))
        var fresh: [Int64: Int64] = [:]
        for (c, j) in order {
            if let old = rowId(chains[c][j]) { fresh[old] = kernel.mintId() }
        }
        lock.lock()
        for (tag, old) in tags {
            if let n = fresh[old] { tags[tag] = n }
        }
        lock.unlock()
        func remint(_ r: JSON) -> JSON {
            guard case .object(var o) = r else { return r }
            if let old = rowId(r), let n = fresh[old] { o["id"] = .integer(n) }
            if let p = rowParent(r), let n = fresh[p] { o["parent"] = .integer(n) }
            return .object(o)
        }
        chains = chains.map { $0.map(remint) }
        return order.map { chains[$0.0][$0.1] }
    }

    /// The ids of a chain's rows (every row of a fixture chain carries one).
    func ids(_ rows: [JSON]) -> [Int64] { rows.compactMap(rowId) }

    /// python `_seed` / `k.rows_put` — straight into the agent's store (ids the
    /// fixture minted, trusted as the store trusts a caller's own).
    func seed(_ rows: [JSON]) {
        // a row with no id takes the kernel's next one (python `rows_put`)
        appendRows(agent, kernel: kernel, rows)
    }

    /// The store key the agent's rows live under.
    var owner: String {
        "\(kernel.root?.metaValue(forKey: "guid")?.asString ?? MOCK_GUID)|\(agent.value)"
    }

    /// python `k.rows_of()` — every row for the agent, views included.
    func rowsOf() -> [JSON] {
        guard let mem = kernel.agent(AgentId("memory")),
            let bundle = kernel.bundles.get("ram_store.tools") as? RamStoreBundle
        else { return [] }
        return bundle.ownerRows(mem, owner: owner)
    }

    /// python `k.folds_of()`.
    func foldsOf() -> [JSON] { rowsOf().filter(isFold) }

    /// python `_ctx(bud, kernel, summarize)` of test_folds: recent_n 4,
    /// self_id the fixture agent, client_id "c" (so `fold` PERSISTS).
    func ctx(
        _ budget: Int64, summarize: (@Sendable ([JSON]) async throws -> String)? = fakeSummary,
        recentN: Int = 4, clientId: String? = "c", anchors: [Int64: Int64]? = nil
    ) -> ProjectionCtx {
        ProjectionCtx(
            budget: budget, recentN: recentN, summarize: summarize.map(summarizer), selfId: agent,
            kernel: kernel, clientId: clientId, anchors: anchors)
    }
}

/// The order python `_interleave` merges chains in — `random.Random(7)` picks a
/// chain per step. The exact sequence is not the claim; determinism is, so a
/// fixed LCG stands in. Returns (chain, index in chain) per merged row.
func interleaveOrder(_ counts: [Int]) -> [(Int, Int)] {
    var ptr = [Int](repeating: 0, count: counts.count)
    var state: UInt64 = 7
    var out: [(Int, Int)] = []
    while true {
        let live = counts.indices.filter { ptr[$0] < counts[$0] }
        if live.isEmpty { break }
        state = state &* 6_364_136_223_846_793_005 &+ 1_442_695_040_888_963_407
        let i = live[Int((state >> 33) % UInt64(live.count))]
        out.append((i, ptr[i]))
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
