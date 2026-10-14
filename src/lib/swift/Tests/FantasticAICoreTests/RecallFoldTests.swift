// A head longer than the budget FOLDS at `context_recall`: a
// store written under a larger window (another model, a planted past) is folded
// the way a turn's `fold` projection folds it, before the fit picks resolutions.
// Twins of python `ai_core/tests/test_view_recall.py` (the recall-fold cases).

import FantasticJSON
import Foundation
import OrderedCollections
import Testing

@testable import FantasticAICore
@testable import FantasticKernel

/// Its own agent id: the stripped set and the incarnation table are
/// process-global, keyed by agent, and suites run in parallel.
private let AI = "ai_rf"

/// Answers a fold summary with a title + body, and the recall's own question
/// with an empty expansion list.
private func foldingModel(_ summaries: AskTally) -> ComputedProvider {
    ComputedProvider { messages in
        if (messages[0]["content"].asString ?? "").contains("Summarize the conversation") {
            summaries.bump()
            return "read files \(summaries.count)\n\nthe files were read."
        }
        return #"{"expand": [], "why": "the folds carry it"}"#
    }
}

/// A model-call counter a `@Sendable` provider closure may bump.
private final class AskTally: @unchecked Sendable {
    private let lock = NSLock()
    private var n = 0
    func bump() {
        lock.lock()
        n += 1
        lock.unlock()
    }
    var count: Int {
        lock.lock()
        defer { lock.unlock() }
        return n
    }
}

/// A head written under a window far larger than this agent's: 20 reads of
/// 3000-char replies (~15k tokens) against a 12k window.
private func longHead(strategy: String) async -> (Kernel, Agent, FoldFixture, AIBackend, Int64) {
    let config = baseConfig { MockProvider() }
    let (kernel, ids, _) = await bootMockKernel(
        config: config, backendIds: [AI], contextWindow: 12000,
        meta: ["output_reserve": .integer(0), "context_strategy": .string(strategy)])
    let agent = kernel.agent(ids[AI]!)!
    let f = FoldFixture(kernel: kernel, agent: AI)
    let goal = f.row("user", "GOAL", "user:goal")
    f.seed([goal] + f.chain("a", 20))
    return (kernel, agent, f, buildAIBackend(config), f.id("user:goal"))
}

private func recall(
    _ backend: AIBackend, _ agent: Agent, _ kernel: Kernel, payload: JSON,
    provider: (any AIProvider)?, clientId: String?
) async throws -> JSON {
    try await KernelTaskLocals.$sender.withValue(agent.id) {
        try await backend.contextRecall(
            agent: agent, payload: payload, kernel: kernel, provider: provider,
            clientId: clientId)
    }
}

@Suite("a head longer than the budget folds at recall", .serialized)
struct RecallFoldTests {
    init() { AIStripStrategy.forget(nil, AI) }

    @Test func recallFoldsAHeadLongerThanTheBudget() async throws {
        let (kernel, agent, f, backend, goal) = await longHead(strategy: "fold")
        let sink = EventSink()
        let inbox = kernel.ensureInbox(agent.id)
        let pump = Task { for await ev in inbox { sink.add(ev.payload) } }
        defer { pump.cancel() }
        let summaries = AskTally()
        let r = try await recall(
            backend, agent, kernel, payload: .object([:]), provider: foldingModel(summaries),
            clientId: "c1")
        let folds = f.foldsOf()
        #expect((r["folded"].asInt ?? 0) >= 1 && r["folded"].asInt == Int64(folds.count), "\(r)")
        #expect(summaries.count == folds.count, "one summary per fold")
        #expect(
            folds.allSatisfy {
                !($0["refs"].asArray ?? []).isEmpty && !($0["title"].asString ?? "").isEmpty
                    && !($0["body"].asString ?? "").isEmpty
            })
        let view = try #require(
            try await KernelTaskLocals.$sender.withValue(agent.id) {
                try await FantasticAICore.loadView(agent.id.value, kernel)
            })
        let ids = view["ids"].asArray ?? []
        #expect(ids.first?["id"].asInt == goal, "the goal turn is always in")
        #expect(ids.contains { $0["res"].asString == "fold" }, "\(view.serialize())")
        // NOTHING FELL OUT UNMARKED: every turn is in the view or under a fold in it
        let rows = f.rowsOf()
        let inView = Set(ids.compactMap { $0["id"].asInt })
        let covered = Set(
            AIFolds.expandIds(
                rows, ids.filter { $0["res"].asString == "fold" }.compactMap { $0["id"].asInt }))
        for t in rows where !["fold", "view", "recall"].contains(t["kind"].asString ?? "") {
            let id = rowId(t) ?? 0
            #expect(inView.contains(id) || covered.contains(id), "row \(id) fell out")
        }
        #expect(r["dropped"].asInt == 0, "\(r)")
        // the turn's caller is told, as by a projection
        try await waitUntil("the context event") { !contextEvents(sink).isEmpty }
        let evs = contextEvents(sink)
        #expect(evs.count == 1)
        #expect(evs.first?["phase"].asString == "compacted")
        #expect(evs.first?["client_id"].asString == "c1")
        #expect(evs.first?["detail"]["folded"].asInt == r["folded"].asInt)
        #expect(evs.first?["detail"]["recall"].asBool == true)
    }

    @Test func aDryRecallOrAnotherStrategyDoesNotFold() async throws {
        let summaries = AskTally()
        let (kernel, agent, f, backend, _) = await longHead(strategy: "fold")
        let r = try await recall(
            backend, agent, kernel, payload: .object(["dry": .bool(true)]),
            provider: foldingModel(summaries), clientId: "c1")
        #expect(r["folded"].asInt == 0 && f.foldsOf().isEmpty, "dry makes no model call: \(r)")
        let (kernel2, agent2, f2, backend2, _) = await longHead(strategy: "truncate")
        let r2 = try await recall(
            backend2, agent2, kernel2, payload: .object([:]), provider: foldingModel(summaries),
            clientId: "c1")
        #expect(r2["folded"].asInt == 0 && f2.foldsOf().isEmpty, "only the fold strategy folds")
        #expect(summaries.count == 0)
    }

    @Test func aHeadThatFitsIsNotFolded() async throws {
        let config = baseConfig { MockProvider() }
        let (kernel, ids, _) = await bootMockKernel(
            config: config, backendIds: [AI], contextWindow: 12000,
            meta: ["output_reserve": .integer(0), "context_strategy": .string("fold")])
        let agent = kernel.agent(ids[AI]!)!
        let f = FoldFixture(kernel: kernel, agent: AI)
        f.seed([f.row("user", "GOAL", "user:goal")] + f.chain("a", 3))
        let summaries = AskTally()
        let r = try await recall(
            buildAIBackend(config), agent, kernel, payload: .object([:]),
            provider: foldingModel(summaries), clientId: "c1")
        #expect(r["folded"].asInt == 0 && summaries.count == 0 && f.foldsOf().isEmpty)
    }
}
