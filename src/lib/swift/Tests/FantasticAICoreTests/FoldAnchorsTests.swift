// A fold carries its own place (`anchor`): it stays in the view when its members
// are not in the rows being rendered (a body rebuilt from the view array, a store
// compacted by `context_forget`), and a store written before the field existed
// renders as it did. Twins of python `ai_core/tests/test_fold_anchors.py`, case
// for case.

import FantasticJSON
import Foundation
import OrderedCollections
import Testing

@testable import FantasticAICore
@testable import FantasticKernel

/// One agent id for the fixture-only cases: the stripped set is process-global,
/// keyed by agent, and suites run in parallel.
private let AI = "ai_anc"

private let DONE = "<call name=\"done\"></call>"
private let CALL = "<call name=\"send\"><target_id>notes</target_id><type>read</type></call>"

/// `contract/ai_core/dump_before_anchors.json` at the repo root — a store made on
/// the code before anchors, shared with python.
private func dumpURL() -> URL {
    // From this file: Tests/FantasticAICoreTests/ → swift → lib → src → repo
    URL(fileURLWithPath: #filePath)
        .deletingLastPathComponent()  // FantasticAICoreTests
        .deletingLastPathComponent()  // Tests
        .deletingLastPathComponent()  // swift
        .deletingLastPathComponent()  // lib
        .deletingLastPathComponent()  // src
        .deletingLastPathComponent()  // repo root
        .appendingPathComponent("contract/ai_core/dump_before_anchors.json")
}

private func loadDump() throws -> JSON {
    try JSON.parse(try Data(contentsOf: dumpURL()))
}

/// `render` as it was before anchors: each fold at the position of the first of
/// its refs present in `rows`. The oracle the new render must match while every
/// member is present. python `_old_render`.
private func oldRender(_ rows: [JSON]) -> [JSON] {
    let cov = AIFolds.covered(rows)
    var firstPos: [Int64: Int] = [:]
    for (i, r) in rows.enumerated() {
        if let rid = rowId(r), cov.contains(rid), firstPos[rid] == nil { firstPos[rid] = i }
    }
    var place: [Int: [JSON]] = [:]
    for r in rows where isFold(r) {
        let inside = rowId(r).map { cov.contains($0) } ?? false
        if let at = refsOf(r).compactMap({ firstPos[$0] }).min(), !inside {
            place[at, default: []].append(r)
        }
    }
    var out: [JSON] = []
    for (i, r) in rows.enumerated() {
        out.append(contentsOf: place[i] ?? [])
        if (rowId(r).map { cov.contains($0) } ?? false) || isFold(r) { continue }
        out.append(r)
    }
    return out
}

private func newRender(_ rows: [JSON]) -> [JSON] {
    AIFolds.render(rows, AIFolds.anchorsOf(rows))
}

private func ids(_ rows: [JSON]) -> [Int64?] { rows.map(rowId) }

private func bytes(_ rows: [JSON]) -> String { JSON.array(rows).serialize() }

/// What the store's `prune` does: the rows go, and each leaves every fold's refs.
/// python `_cascade`.
private func cascade(_ rows: [JSON], _ gone: Set<Int64>) -> [JSON] {
    rows.compactMap { r in
        if let id = rowId(r), gone.contains(id) { return nil }
        if isFold(r) {
            return withField(r, "refs", .array(refsOf(r).filter { !gone.contains($0) }.map(JSON.integer)))
        }
        return r
    }
}

/// The row without `key` (a fold written before that field existed).
private func without(_ row: JSON, _ keys: Set<String>) -> JSON {
    guard case .object(var o) = row else { return row }
    for k in keys { o.removeValue(forKey: k) }
    return .object(o)
}

/// The `fold` projection as a turn runs it, `body` being the whole head (python
/// `_fold_over_head`): anchors from the body, the body seeded into the agent's
/// store, run as the agent.
private func foldOverHead(_ body: [JSON], _ c0: ProjectionCtx) async throws -> AIProjection {
    var c = c0
    c.anchors = AIFolds.anchorsOf(body)
    if let k = c.kernel { appendRows(c.selfId, kernel: k, body.filter { rowId($0) != nil }) }
    let ctx = c
    return try await KernelTaskLocals.$sender.withValue(ctx.selfId) {
        try await AIStrategies.getStrategy("fold")!(body, [], .object([:]), ctx)
    }
}

// ── the fixtures of the fold tests, members present ─────────────────

private func fixtures() -> [(String, [JSON])] {
    var out: [(String, [JSON])] = []
    do {
        let f = FoldFixture(agent: AI)
        let rows = f.chain("a", 2, big: false) + [f.row("user", "later", "user:l")]
        out.append(("one fold", rows + [f.fold(f.ids(Array(rows.prefix(5))), "a", "b", false)]))
    }
    do {
        let f = FoldFixture(agent: AI)
        let rows = f.chain("a", 1, big: false)
        let inner = f.fold(f.ids(rows), "inner", "b", false)
        out.append(("fold of folds", rows + [inner, f.fold([rowId(inner)!], "outer", "b", false)]))
    }
    do {
        let f = FoldFixture(agent: AI)
        let a = f.chain("a", 2, big: false)
        out.append((
            "overlapping",
            a + [
                f.fold(f.ids(Array(a.prefix(3))), "part", "b", false),
                f.fold(f.ids(a), "whole", "b", false),
            ]
        ))
    }
    do {
        let f = FoldFixture(agent: AI)
        let a = f.chain("a", 1, big: false)
        out.append(("a member gone", a + [f.fold(f.ids(a) + [f.id("gone:0000")], "done", "b", false)]))
    }
    do {
        let f = FoldFixture(agent: AI)
        let a = f.chain("a", 1, big: false)
        let fd = f.fold(f.ids(a), "done", "b", false)
        let late = f.row("user", "[reply to notes:late]\nlate", "ai:late", "ai:a0")
        out.append(("a late reply", a + [fd, late]))
    }
    do {
        let f = FoldFixture(agent: AI)
        var chs = "abcdef".map { f.chain(String($0), 1, big: false) }
        let goal = f.row("user", "GOAL", "user:goal")
        var rows = [goal] + f.interleave(&chs)
        rows += chs.map { f.fold(f.ids($0), "d", "S", false) }
        out.append(("interleaved chains", rows + [f.row("user", "now?", "user:now")]))
    }
    do {
        let f = FoldFixture(agent: AI)
        let goal = f.row("user", "GOAL", "user:goal")
        var chs = [f.chain("a", 1, big: false), f.chain("b", 1, big: false)]
        let body = [goal] + f.interleave(&chs)
        let fa = f.fold(f.ids(chs[0]), "a", "b", false)
        let fb = f.fold(f.ids(chs[1]), "b", "b", false)
        let tail = (0..<3).map { f.row("user", "t\($0)", "user:t\($0)") }
        out.append(("two folds and a tail", body + [fa, fb] + tail))
    }
    return out
}

// ── through a real agent and its ram_store ──────────────────────────

/// Answers a turn with `done`, a fold summary with a title and a body, and holds
/// a turn open while held. python `_Model`.
private final class AnchorModel: @unchecked Sendable {
    private let lock = NSLock()
    private var held = false
    private var entered = false
    private var requests: [[JSON]] = []

    func hold() { lock.withLock { held = true } }
    func release() { lock.withLock { held = false } }
    var isHeld: Bool { lock.withLock { held } }
    var hasEntered: Bool { lock.withLock { entered } }
    var requestCount: Int { lock.withLock { requests.count } }

    func record(_ messages: [JSON]) {
        lock.withLock {
            requests.append(messages)
            entered = true
        }
    }
}

private final class AnchorModelProvider: AIProvider, @unchecked Sendable {
    let model = "stub"
    private let m: AnchorModel
    init(_ m: AnchorModel) { self.m = m }

    func chat(messages: [JSON]) -> AsyncThrowingStream<AIChunk, Error> {
        if (messages.first?["content"].asString ?? "").contains("Summarize the conversation") {
            return AsyncThrowingStream { c in
                c.yield(.token("the work, folded\n\nwhat was read and how it ended."))
                c.finish()
            }
        }
        let gate = self.m
        gate.record(messages)
        return AsyncThrowingStream { c in
            Task {
                while gate.isHeld { try? await Task.sleep(nanoseconds: 5_000_000) }
                c.yield(.token(DONE))
                c.finish()
            }
        }
    }
}

/// A booted kernel with ONE fold-strategy agent `id` on the model, and a backend
/// built from the same config to assemble prompts with. python `_agent`.
private func bootAgent(_ id: String, window: Int64 = 32768, model: AnchorModel) async
    -> (Kernel, AgentId, AIBackend)
{
    let config = baseConfig { AnchorModelProvider(model) }
    let (kernel, agentIds, _) = await bootMockKernel(
        config: config, backendIds: [id], contextWindow: window,
        meta: [
            "output_reserve": .integer(0), "context_strategy": .string("fold"),
            "system_prompt": .string("You are a test agent."),
        ])
    AIStripStrategy.forget(nil, id)
    return (kernel, agentIds[id]!, buildAIBackend(config))
}

/// A closed chain: task → send → reply → done. python `_closed`.
private func closed(_ kernel: Kernel, _ tag: String, big: Int = 3000) -> [JSON] {
    let u = kernel.mintId()
    let a = kernel.mintId()
    let r = kernel.mintId()
    let d = kernel.mintId()
    let words = (0..<(big / 4)).map { "\(tag)\($0)" }.joined(separator: " ")
    return [
        .object([
            "role": .string("user"), "id": .integer(u), "parent": .null,
            "content": .string("task \(tag)"),
        ]),
        .object([
            "role": .string("assistant"), "id": .integer(a), "parent": .integer(u),
            "content": .string("\(tag) \(CALL)"),
        ]),
        .object([
            "role": .string("user"), "id": .integer(r), "parent": .integer(a),
            "content": .string("[reply to notes \u{b7} read \u{b7} \"\(tag)\"]\n" + words),
        ]),
        .object([
            "role": .string("assistant"), "id": .integer(d), "parent": .integer(r),
            "content": .string("ok \(DONE)"),
        ]),
    ]
}

private func goalRow(_ kernel: Kernel) -> JSON {
    .object([
        "role": .string("user"), "id": .integer(kernel.mintId()), "parent": .null,
        "content": .string("GOAL"),
    ])
}

/// python `_seed`: `_save_history` as the agent.
private func seed(_ kernel: Kernel, _ id: AgentId, _ rows: [JSON]) async throws {
    try await KernelTaskLocals.$sender.withValue(id) {
        try await saveHistory(id.value, kernel, rows)
    }
}

/// python `_call` / `_turn`: one send, its result.
private func call(_ kernel: Kernel, _ id: AgentId, _ payload: OrderedDictionary<String, JSON>) async
    -> JSON
{
    let out = await kernel.send([Item(id, .object(payload))])[0]
    #expect(out.status == .completed, "\(out.result.serialize())")
    return out.result
}

private func turn(_ kernel: Kernel, _ id: AgentId, _ text: String = "go") async -> JSON {
    await call(kernel, id, ["type": .string("send"), "text": .string(text)])
}

/// Every row of the agent's store, read as the agent. python `_store`.
private func store(_ kernel: Kernel, _ id: AgentId) async throws -> [JSON] {
    try await KernelTaskLocals.$sender.withValue(id) {
        try await storeCall(id.value, kernel, ["type": .string("read")])
    }["rows"].asArray ?? []
}

/// The next prompt as the model reads it. python `_prompt`.
private func prompt(_ kernel: Kernel, _ id: AgentId, _ backend: AIBackend) async throws -> String {
    let agent = kernel.agent(id)!
    let (msgs, _) = try await KernelTaskLocals.$sender.withValue(id) {
        try await backend.assemble(agent: agent, userText: "next", systemPrompt: nil, kernel: kernel)
    }
    return JSON.array(msgs.map { .object(["role": $0["role"], "content": $0["content"]]) }).serialize()
}

private func views(_ kernel: Kernel, _ id: AgentId) async throws -> [JSON] {
    try await store(kernel, id).filter { $0["kind"].asString == "view" }
}

/// Every fold named in `ids`, and every fold under one of them. python `_reaches`.
private func reaches(_ rows: [JSON], _ ids: [Int64]) -> Set<Int64> {
    var byId: [Int64: JSON] = [:]
    for r in rows { if let id = rowId(r) { byId[id] = r } }
    var out: Set<Int64> = []
    var stack = ids
    while let x = stack.popLast() {
        guard let r = byId[x], isFold(r), !out.contains(x) else { continue }
        out.insert(x)
        stack.append(contentsOf: refsOf(r))
    }
    return out
}

/// The live backend that serves the kernel's agents (its view source is RAM).
private func liveBackend(_ kernel: Kernel) -> AIBackend? {
    (kernel.bundles.get("mock_backend.tools") as? MockBundle)?.core
}

@Suite("fold anchors: a fold carries its own place", .serialized)
struct FoldAnchorsTests {
    init() { AIStripStrategy.forget(nil, AI) }

    @Test func renderIsUnchangedWhenMembersArePresent() {
        for (name, rows) in fixtures() {
            #expect(bytes(newRender(rows)) == bytes(oldRender(rows)), "\(name)")
        }
    }

    /// The committed dump, made on the code before anchors: every member present.
    @Test func renderIsUnchangedForAStoreWrittenBeforeAnchors() throws {
        let dump = try loadDump()
        var refs: [Int64: JSON] = [:]
        for fr in dump["tables"]["fold_refs"].asArray ?? [] {
            if let fid = fr["fold"].asInt { refs[fid] = fr["members"] }
        }
        let rows: [JSON] = (dump["tables"]["rows"].asArray ?? []).map { t in
            let r = t["row"]
            if let id = rowId(r), let m = refs[id] { return withField(r, "refs", m) }
            return r
        }
        let last = try #require(rows.lastIndex { $0["kind"].asString == "clear" })
        let head = rows[last...].filter { !["view", "anchors"].contains($0["kind"].asString ?? "") }
        #expect(bytes(newRender(head)) == bytes(oldRender(head)))
    }

    /// A run as the product lives it: chains arrive, each projection folds what is
    /// over the budget, and every fold lands in the store after its members and
    /// before what arrives next — folds of folds included.
    @Test func renderIsUnchangedOverFoldsTheStrategyWrote() async throws {
        let f = FoldFixture(agent: AI)
        var rows = [f.row("user", "GOAL", "user:goal")]
        for t in "abcdefghijkl" {
            rows += f.chain(String(t), 2)
            _ = try await foldOverHead(rows, f.ctx(4000))
            let mine = Set(rows.compactMap(rowId))
            rows += f.rowsOf().filter { r in rowId(r).map { !mine.contains($0) } ?? false }
        }
        #expect(rows.contains { isFold($0) && ($0["level"].asInt ?? 1) >= 2 })
        #expect(bytes(newRender(rows)) == bytes(oldRender(rows)))
    }

    // ── a new fold carries its anchor ───────────────────────────────────

    @Test func newFoldsCarryAnAnchor() async throws {
        let f = FoldFixture(agent: AI)
        let goal = f.row("user", "GOAL", "user:goal")
        var chs = "abc".map { f.chain(String($0), 1, big: false) }
        var rows = [goal] + f.interleave(&chs)
        f.seed(rows)  // the store refuses a fold whose refs name rows it does not hold
        func indexOf(_ r: JSON) -> Int { rows.firstIndex { rowId($0) == rowId(r) }! }
        var anchors: [Int64: Int64] = [:]
        var l1: [JSON] = []
        for ch in chs {
            let idxs = ch.map(indexOf)
            let (snapshot, known) = (rows, anchors)
            let fd = try await KernelTaskLocals.$sender.withValue(f.agent) {
                try await AIFoldStrategy.writeFold(f.ctx(1_000_000), snapshot, idxs, known)
            }
            #expect(fd["anchor"].asInt == ch.compactMap(rowId).min(), "an L1's anchor is its first leaf")
            anchors[rowId(fd)!] = fd["anchor"].asInt!
            rows.append(fd)
            l1.append(fd)
        }
        let idxs = l1.map(indexOf)
        let (snapshot, known) = (rows, anchors)
        let l2 = try await KernelTaskLocals.$sender.withValue(f.agent) {
            try await AIFoldStrategy.writeFold(f.ctx(1_000_000), snapshot, idxs, known)
        }
        #expect(l2["level"].asInt == 2)
        #expect(l2["anchor"].asInt == l1.compactMap { $0["anchor"].asInt }.min(), "an L2's is its members' least")
        var stored: [Int64: JSON] = [:]
        for r in f.rowsOf() { if let id = rowId(r) { stored[id] = r } }
        #expect((l1 + [l2]).allSatisfy { stored[rowId($0)!]?["anchor"] == $0["anchor"] })
    }

    // ── a fold without its members ──────────────────────────────────────

    @Test func aFoldWithoutMembersKeepsItsPlace() {
        let f = FoldFixture(agent: AI)
        let goal = f.row("user", "GOAL", "user:goal")
        var chs = [f.chain("a", 1, big: false), f.chain("b", 1, big: false)]
        let ab = f.interleave(&chs)
        let (a, b) = (chs[0], chs[1])
        let c = f.chain("c", 1, big: false)
        let fa = f.fold(f.ids(a), "a", "b", false)
        let fb = f.fold(f.ids(b), "b", "b", false)
        let fc = f.fold(f.ids(c), "c", "b", false)
        let l2 = f.fold([rowId(fa)!, rowId(fb)!], "a and b", "b", false)
        let tail = (0..<2).map { f.row("user", "t\($0)", "user:t\($0)") }
        let rows = [goal] + ab + c + [fa, fb, fc, l2] + tail
        let before = ids(newRender(rows))
        #expect(before == [rowId(goal), rowId(l2), rowId(fc)] + ids(tail))
        // the leaves of a and b go: the L2 keeps its place, ahead of c's fold
        var after = cascade(rows, Set(f.ids(a + b)))
        #expect(ids(newRender(after)) == before)
        // c's leaves go too, all but its last: c's fold stays where its first leaf was
        after = cascade(after, Set(f.ids(Array(c.dropLast()))))
        #expect(ids(newRender(after)) == before)
        // a fold whose first member is gone, others left: it does not move to a survivor
        let x = f.row("user", "x", "user:x")
        let y = f.row("assistant", "y", "user:y")
        let z = f.row("user", "z", "user:z")
        let fx = f.fold([rowId(x)!, rowId(z)!], "x and z", "b", false)
        let xs = [x, y, z, fx]
        #expect(ids(newRender(xs)) == [rowId(fx), rowId(y)])
        #expect(ids(newRender(cascade(xs, [rowId(x)!]))) == [rowId(fx), rowId(y)])
    }

    @Test func anUnanchoredFoldWithoutMembersIsCountedNotPlaced() {
        let f = FoldFixture(agent: AI)
        let a = f.chain("a", 1, big: false)
        let old = without(f.fold(f.ids(a), "done", "b", false), ["anchor"])  // written before the field
        let tail = [f.row("user", "now", "user:now")]
        let rows = cascade(a + [old] + tail, Set(f.ids(a)))
        var anchors = AIFolds.anchorsOf(rows)
        #expect(anchors[rowId(old)!] == nil)
        #expect(ids(AIFolds.render(rows, anchors)) == ids(tail))
        #expect(AIFolds.unanchored(rows, anchors) == 1)
        // an `anchors` row recorded for it places it again
        let rec: JSON = .object([
            "kind": .string("anchors"), "v": .integer(1),
            "anchors": .object([String(rowId(old)!): .integer(rowId(a[0])!)]),
        ])
        anchors = AIFolds.anchorsOf(rows + [rec])
        #expect(ids(AIFolds.render(rows, anchors)) == [rowId(old)] + ids(tail))
    }

    @Test func theProjectionCountsUnanchoredFolds() async throws {
        let f = FoldFixture(agent: AI)
        let a = f.chain("a", 1, big: false)
        let old = without(f.fold(f.ids(a), "done", "b", false), ["anchor", "refs"])
        let body = [f.row("user", "GOAL", "user:goal"), old, f.row("user", "now", "user:now")]
        let proj = try await foldOverHead(body, f.ctx(1_000_000))
        #expect(proj.unanchored == 1)
        #expect(!ids(proj.body).contains(rowId(old)))
    }

    // ── through a real agent and its ram_store ──────────────────────────

    /// The bug: a turn rebuilt from the view array holds folds without their
    /// members, and the next projection past TRIGGER rendered them nowhere.
    @Test func foldsSurviveAProjectionOverTheViewArray() async throws {
        let model = AnchorModel()
        let (k, id, _) = await bootAgent("anc_fv", window: 4000, model: model)
        // The boot owes a recall, which would fold and write the view before the
        // turn's projection does; python's fixture creates the agent unbooted, so
        // the first view is the projection's. Settle the owed recall the same way.
        #expect(liveBackend(k)?.takeRecallDue(id.value) == true, "the boot owes a recall")
        try await seed(k, id, [goalRow(k)] + "abcdef".flatMap { closed(k, String($0), big: 2000) })
        _ = await turn(k, id)
        var rows = try await store(k, id)
        let first = rows.filter(isFold).compactMap(rowId)
        #expect(!first.isEmpty, "the first turn folded")
        #expect(try await views(k, id).last?["why"].asString == "projection: fold")
        // the next turn's prompt is rebuilt from that view array, and is pushed past
        // TRIGGER by what lands after it
        try await seed(k, id, "ghij".flatMap { closed(k, String($0), big: 2000) })
        _ = await turn(k, id)
        #expect(liveBackend(k)?.getViewSource(id.value) == "array")
        rows = try await store(k, id)
        #expect(rows.filter(isFold).count > first.count, "it folded again")
        let shown = (try await views(k, id).last?["ids"].asArray ?? []).compactMap { $0["id"].asInt }
        let reached = reaches(rows, shown)
        let missing = first.filter { !reached.contains($0) }
        #expect(missing.isEmpty, "folds \(missing) fell out of the view \(shown)")
        #expect(first.allSatisfy { shown.contains($0) }, "\(first) \(shown)")
    }

    @Test func contextForgetPrunesCoveredRowsAndOldViews() async throws {
        let model = AnchorModel()
        let (k, id, backend) = await bootAgent("anc_ff", window: 4000, model: model)
        try await seed(k, id, [goalRow(k)] + "abcdef".flatMap { closed(k, String($0), big: 2000) })
        _ = await turn(k, id)
        try await seed(k, id, "ghij".flatMap { closed(k, String($0), big: 2000) })
        _ = await turn(k, id, "and again")
        let rows = try await store(k, id)
        let foldRows = rows.filter(isFold)
        #expect(!foldRows.isEmpty)
        #expect(foldRows.allSatisfy { if case .integer = $0["anchor"] { return true } else { return false } })
        let covered = Set(AIFolds.expandIds(rows, foldRows.compactMap(rowId)))
        let viewIds = rows.filter { $0["kind"].asString == "view" }.compactMap(rowId)
        #expect(viewIds.count >= 2)
        let before = try await prompt(k, id, backend)
        let out = await call(k, id, ["type": .string("context_forget")])
        #expect(
            out == .object([
                "pruned": .integer(Int64(covered.count + viewIds.count - 1)),
                "kept_folds": .integer(Int64(foldRows.count)),
                "unanchored": .integer(0),
            ]), "\(out.serialize())")
        let afterRows = try await store(k, id)
        let left = Set(afterRows.compactMap(rowId))
        #expect(covered.isDisjoint(with: left), "every row a fold summarises is gone")
        #expect(Set(foldRows.compactMap(rowId)).isSubset(of: left), "every fold is kept")
        #expect(
            afterRows.filter { $0["kind"].asString == "view" }.compactMap(rowId) == Array(viewIds.suffix(1)))
        #expect(
            !afterRows.contains { $0["kind"].asString == "anchors" },
            "folds that carry their anchor need no anchors row")
        // the head cache is the store, no restart
        let head = AIMemory.of(k).with { $0.heads[id.value] } ?? []
        #expect(bytes(head.map { without($0, ["agent"]) }) == bytes(afterRows.map { without($0, ["agent"]) }))
        #expect(try await prompt(k, id, backend) == before, "the next prompt is the same bytes")
        // a second forget has nothing left to do
        let again = await call(k, id, ["type": .string("context_forget")])
        #expect(again["pruned"].asInt == 0, "\(again.serialize())")
        // a pruned id is gone for recall and history; the fold still says what it was
        let gone = try #require(covered.min())
        let h = await call(k, id, ["type": .string("history"), "fold": foldRows[0]["id"]])
        #expect(h["fold"]["title"] == foldRows[0]["title"] && pyTruthy(h["fold"]["summary"]), "\(h.serialize())")
        #expect(!(h["rows"].asArray ?? []).contains { rowId($0) == gone })
    }

    @Test func contextForgetKeepsOpenChainsAndUnanchoredFolds() async throws {
        let model = AnchorModel()
        let (k, id, _) = await bootAgent("anc_fo", model: model)
        let goal = goalRow(k)
        let done = closed(k, "done", big: 10)
        // an open chain: its last row is a send still owed a reply; its head is folded
        var o = Array(closed(k, "open", big: 10).prefix(3))
        o.append(.object([
            "role": .string("assistant"), "id": .integer(k.mintId()), "parent": o[2]["id"],
            "content": .string(CALL),
        ]))
        // a fold written before the field, with its members: anchored by deriving
        let legacyM = closed(k, "legacy", big: 10)
        try await seed(k, id, [goal] + done + o + legacyM)
        let fDone = AIFolds.makeFold(
            refs: done.compactMap(rowId), title: "d", body: "b", degraded: false,
            id: k.mintId(), anchor: rowId(done[0])!)
        let fOpen = AIFolds.makeFold(
            refs: o.prefix(2).compactMap(rowId), title: "o", body: "b", degraded: false,
            id: k.mintId(), anchor: rowId(o[0])!)
        let legacy = without(
            AIFolds.makeFold(
                refs: legacyM.compactMap(rowId), title: "l", body: "b", degraded: false,
                id: k.mintId(), anchor: 0), ["anchor"])
        // and one with neither field nor members: its place cannot be known
        let lost = without(
            AIFolds.makeFold(refs: [], title: "lost", body: "b", degraded: false, id: k.mintId(), anchor: 0),
            ["anchor"])
        try await seed(k, id, [fDone, fOpen, legacy, lost])
        let out = await call(k, id, ["type": .string("context_forget")])
        #expect(
            out == .object([
                "pruned": .integer(Int64(done.count + legacyM.count)),
                "kept_folds": .integer(4),
                "unanchored": .integer(1),
            ]), "\(out.serialize())")
        var left: [Int64: JSON] = [:]
        for r in try await store(k, id) { if let rid = rowId(r) { left[rid] = r } }
        #expect(o.allSatisfy { left[rowId($0)!] != nil }, "an open chain keeps every row")
        #expect([fDone, fOpen, legacy, lost].allSatisfy { left[rowId($0)!] != nil })
        let recorded = left.values.filter { $0["kind"].asString == "anchors" }
        #expect(recorded.count == 1)
        #expect(
            recorded.map { AIFolds.recordedAnchors([$0]) } == [[rowId(legacy)!: rowId(legacyM[0])!]],
            "the legacy fold's place is recorded before its members go")
    }

    @Test func aDumpFromBeforeAnchorsRestoresWithTheSameView() async throws {
        let dump = try loadDump()
        let aid = try #require(dump["agent"].asString)
        let model = AnchorModel()
        let (k, id, backend) = await bootAgent(aid, model: model)
        k.update(AgentId("kernel_state"), ["guid": dump["guid"]])
        let top = (dump["tables"]["rows"].asArray ?? []).compactMap { $0["row"]["id"].asInt }.max() ?? 0
        while k.mintId() <= top {}  // the kernel's counter is past every stored id
        let res = await k.send([
            Item(AgentId("memory"), .object(["type": .string("inject"), "tables": dump["tables"]]))
        ])[0]
        #expect(res.status == .completed, "\(res.result.serialize())")
        forgetHead(aid, k)
        let renderIds = dump["render_ids"]
        func rendered() async throws -> JSON {
            let head = try await KernelTaskLocals.$sender.withValue(id) { try await loadHead(aid, k) }
            let hist = try await KernelTaskLocals.$sender.withValue(id) { try await loadHistory(aid, k) }
            return .array(ids(AIFolds.render(hist, AIFolds.anchorsOf(head))).map { $0.map(JSON.integer) ?? .null })
        }
        #expect(try await rendered() == renderIds, "the view main rendered")
        let agent = k.agent(id)!
        let (msgs, _) = try await KernelTaskLocals.$sender.withValue(id) {
            try await backend.assemble(agent: agent, userText: "next", systemPrompt: nil, kernel: k)
        }
        #expect(JSON.array(msgs.dropFirst().dropLast().map { $0["id"] }) == dump["prompt_ids"])
        let before = try await prompt(k, id, backend)
        let rows = try await store(k, id)
        let lastClear = try #require(rows.filter { $0["kind"].asString == "clear" }.compactMap(rowId).max())
        let headFolds = rows.filter { isFold($0) && (rowId($0) ?? 0) > lastClear }
        #expect(!headFolds.isEmpty && !headFolds.contains { $0.asObject?["anchor"] != nil })
        let members = Set(AIFolds.expandIds(rows, headFolds.compactMap(rowId)))
        let out = await call(k, id, ["type": .string("context_forget")])
        #expect(
            out["unanchored"].asInt == 0 && out["kept_folds"].asInt == Int64(headFolds.count),
            "\(out.serialize())")
        #expect(try await prompt(k, id, backend) == before, "the next prompt is the same bytes")
        let left = try await store(k, id)
        #expect(members.isDisjoint(with: Set(left.compactMap(rowId))), "the old folds' members are gone")
        let recorded = left.filter { $0["kind"].asString == "anchors" }
        #expect(recorded.count == 1)
        let all = AIFolds.anchorsOf(rows)
        var want: [Int64: Int64] = [:]
        for f in headFolds { if let fid = rowId(f) { want[fid] = all[fid] } }
        #expect(AIFolds.recordedAnchors(recorded) == want)
        #expect(try await rendered() == renderIds)
        // dump → inject into a fresh store under the same guid: still the same view
        let tables = await k.send([Item(AgentId("memory"), .object(["type": .string("dump")]))])[0]
        _ = await k.send([
            Item(
                AgentId("kernel_state"),
                .object([
                    "type": .string("create_agent"), "parent_id": .string("kernel_state"),
                    "handler_module": .string("ram_store.tools"), "id": .string("memory2"),
                    "callable_by": .object([aid: .string("*")]),
                ]))
        ])[0]
        var grant = k.agent(id)?.metaValue(forKey: "can_call")?.asObject ?? [:]
        grant["memory2"] = .string("*")
        k.update(id, ["can_call": .object(grant), "memory_store_id": .string("memory2")])
        let res2 = await k.send([
            Item(
                AgentId("memory2"),
                .object(["type": .string("inject"), "tables": tables.result["tables"]]))
        ])[0]
        #expect(res2.status == .completed, "\(res2.result.serialize())")
        forgetHead(aid, k)
        #expect(try await rendered() == renderIds)
        #expect(try await prompt(k, id, backend) == before)
    }

    @Test func contextForgetIncludeCleared() async throws {
        let model = AnchorModel()
        let (k, id, _) = await bootAgent("anc_fc", model: model)
        let old = closed(k, "old", big: 10)
        try await seed(k, id, old)
        let cleared = await call(k, id, ["type": .string("context_clear"), "why": .string("next")])
        let new = closed(k, "new", big: 10)
        try await seed(k, id, new)
        let kept = await call(k, id, ["type": .string("context_forget")])
        var left = Set(try await store(k, id).compactMap(rowId))
        #expect(Set(old.compactMap(rowId)).isSubset(of: left), "without include_cleared nothing behind")
        let out = await call(k, id, ["type": .string("context_forget"), "include_cleared": .bool(true)])
        left = Set(try await store(k, id).compactMap(rowId))
        #expect(Set(old.compactMap(rowId)).isDisjoint(with: left), "the rows before the clear go")
        #expect(cleared["cleared"].asInt.map { left.contains($0) } == true, "the clear row stays")
        #expect(Set(new.compactMap(rowId)).isSubset(of: left))
        #expect(
            out["pruned"].asInt == Int64(old.count) && kept["pruned"].asInt == 0,
            "\(kept.serialize()) \(out.serialize())")
        let bad = await k.send([
            Item(id, .object(["type": .string("context_forget"), "include_cleared": .string("yes")]))
        ])[0].result
        #expect(
            bad["reason"].asString == "bad_arg" && bad["arg"].asString == "include_cleared",
            "\(bad.serialize())")
    }

    @Test func contextForgetWaitsForARunningTurn() async throws {
        let model = AnchorModel()
        model.hold()
        let (k, id, _) = await bootAgent("anc_fw", model: model)
        let running = Task { await turn(k, id) }
        try await waitUntil("the turn reached the model") { model.hasEntered }
        let forget = Task { await call(k, id, ["type": .string("context_forget")]) }
        let finished = FinishFlag()
        let watcher = Task {
            _ = await forget.value
            finished.set()
        }
        try await Task.sleep(nanoseconds: 100_000_000)
        #expect(!finished.isSet, "it waits for the turn")
        model.release()
        _ = await running.value
        let out = await forget.value
        _ = await watcher.value
        #expect((out["pruned"].asInt ?? -1) >= 0, "\(out.serialize())")
    }

    // ── a new fold's anchor: known for every member, or the fold fails loud ──

    @Test func aNewFoldsAnchorIsItsLeastLeaf() throws {
        let rows: [JSON] = [
            .object(["role": .string("user"), "id": .integer(7), "content": .string("a")]),
            .object([
                "role": .string("user"), "id": .integer(30), "kind": .string("fold"),
                "refs": .array([.integer(3), .integer(4)]), "anchor": .integer(3),
                "content": .string("f"),
            ]),
            .object([
                "role": .string("user"), "id": .integer(31), "kind": .string("fold"),
                "refs": .array([.integer(1)]), "content": .string("legacy"),
            ]),
        ]
        // a member fold places by its own anchor, a legacy one by the head's anchors
        let anchors: [Int64: Int64] = [31: 1]
        #expect(try AIFolds.memberAnchor(rows, [0], anchors) == 7)
        #expect(try AIFolds.memberAnchor(rows, [0, 1], anchors) == 3)
        #expect(try AIFolds.memberAnchor(rows, [0, 1, 2], anchors) == 1)
    }

    @Test func aFoldThatCannotBePlacedIsAnError() {
        let rows: [JSON] = [
            .object([
                "role": .string("user"), "id": .integer(31), "kind": .string("fold"),
                "refs": .array([.integer(1)]), "content": .string("legacy"),
            ]),
            .object(["role": .string("user"), "id": .string("x"), "content": .string("a row with no int id")]),
            .object(["role": .string("user"), "content": .string("a row with no id")]),
        ]
        let cases: [([Int], String)] = [
            ([0], "fold: member fold 31 has no anchor"),
            ([1], "fold: member row 'x' has no int id"),
            ([2], "fold: member row None has no int id"),
            ([], "fold: a fold needs at least one member"),
        ]
        for (idxs, msg) in cases {
            #expect(throws: PyError("ValueError", msg)) {
                _ = try AIFolds.memberAnchor(rows, idxs, [:])
            }
        }
    }
}

/// A flag a `@Sendable` task may raise.
private final class FinishFlag: @unchecked Sendable {
    private let lock = NSLock()
    private var on = false
    func set() { lock.withLock { on = true } }
    var isSet: Bool { lock.withLock { on } }
}
