// Deterministic, no-LLM tests for the context-budget primitives, the overflow
// strategies (the projection logic), and the Context Protocol seam (the unified
// notice, the `context` events, the backstop, the goal pin). Twins of the
// seam/notice/backstop/goal-pin cases of python `ai_core/tests/test_context.py`;
// the trimmer cases live in `HistoryViewTests`, the recall/reaction cases in
// `AIBackendTests`.

import FantasticJSON
import FantasticKernel
import Foundation
import OrderedCollections
import Testing

@testable import FantasticAICore

/// One agent id per suite: the stripped set is process-global, keyed by agent.
private let AI = "ai_c"

private func turn(_ role: String, _ content: String) -> JSON { plainTurn(role, content) }

/// python `_body(n, size=200)`.
private func body(_ n: Int, size: Int = 200) -> [JSON] {
    (0..<n).map { turn($0 % 2 == 0 ? "user" : "assistant", "turn\($0)-" + String(repeating: "x", count: size)) }
}

/// python `_msgs(n)`.
private func msgs(_ n: Int) -> [JSON] { [turn("system", "sys")] + body(n) }

@Sendable private func fakeSummaryS(_ msgs: [JSON]) async throws -> String { "SUMMARY" }

/// python `_ctx(bud, recent_n, summarize)`.
private func ctx(
    _ bud: Int64, recentN: Int = 4,
    summarize: (@Sendable ([JSON]) async throws -> String)? = fakeSummaryS,
    selfId: String = AI
) -> ProjectionCtx {
    ProjectionCtx(budget: bud, recentN: recentN, summarize: summarize, selfId: AgentId(selfId), kernel: Kernel())
}

/// python `_FakeKernel` + the record: a booted kernel whose `ai_c` agent carries
/// the given context meta, a backend to drive the seam, and a sink on the
/// agent's inbox for the `context` events.
private func seam(
    window: Int64 = 32768, meta: OrderedDictionary<String, JSON> = ["output_reserve": .integer(0)]
) async -> (Kernel, Agent, AIBackend, EventSink, Task<Void, Never>) {
    let config = baseConfig { .provider(MockProvider(tokens: ["FAKESUMMARY"])) }
    let (kernel, ids, _) = await bootMockKernel(config: config, backendIds: [AI], contextWindow: window, meta: meta)
    let agent = kernel.agent(ids[AI]!)!
    let sink = EventSink()
    let inbox = kernel.ensureInbox(agent.id)
    let pump = Task { for await ev in inbox { sink.add(ev) } }
    return (kernel, agent, buildAIBackend(config), sink, pump)
}

/// A meta-only agent, for the record-reading primitives.
private func rec(_ meta: OrderedDictionary<String, JSON>) -> Agent {
    Agent(id: AgentId("rec"), handlerModule: nil, parentId: nil, meta: meta)
}

@Suite("the context seam", .serialized)
struct ContextSeamTests {
    init() { AIStripStrategy.forget(AI) }

    // ── primitives ──────────────────────────────────────────────────────

    @Test func estimateScaleAndMonotonic() {
        #expect(AIContext.estimateOne(turn("user", String(repeating: "x", count: 400))) >= 100)  // ~400/4
        #expect(AIContext.estimateTokens(body(4)) > AIContext.estimateTokens(body(2)))
    }

    @Test func windowPrecedence() {
        #expect(AIContext.resolveContextWindow(rec(["context_window": .integer(8000), "num_ctx": .integer(2000)])) == 8000)
        #expect(AIContext.resolveContextWindow(rec(["num_ctx": .integer(2000)])) == 2000)
        #expect(AIContext.resolveContextWindow(rec([:])) == AIContext.defaultContextWindow)
    }

    @Test func budgetFloor() {
        #expect(AIContext.budget(rec(["context_window": .integer(100), "output_reserve": .integer(1000)])) == 256)
    }

    @Test func registryNoMemgpt() {
        // memgpt collapsed into the universal seam notice — it is no longer a strategy.
        #expect(Set(AIStrategies.strategyNames()) == ["compact", "truncate", "strip", "fold"])
        #expect(AIStrategies.defaultStrategy == "compact")
        #expect(AIStrategies.getStrategy("memgpt") == nil)
        #expect(AIStrategies.getStrategy("nope") == nil)  // unknown ⇒ nil (caller errors), no default
    }

    // ── strategies (return a Projection, NOT a fabricated turn) ─────────

    @Test func compactReturnsProjectionWithSummary() async {
        let b = body(10)
        let proj = await AIStrategies.getStrategy("compact")!(b, [turn("system", "S")], .object([:]), ctx(1_000_000))
        #expect(proj.summary == "SUMMARY")  // artifact, not a fabricated turn
        // The task-framing turn is PINNED ahead of the recent window (see
        // `everyStrategyKeepsTheGoalWhenTheBodyOverflows` for why). Everything after
        // it is the recent window, verbatim.
        #expect(proj.body == [b[0]] + b.suffix(4))
        // the strategy never fabricates the old leading marker turn
        #expect(!proj.body.contains { ($0["content"].asString ?? "").contains("[Earlier conversation summary]") })
    }

    @Test func truncateReturnsProjectionNoSummarizer() async {
        final class Spy: @unchecked Sendable { var called = 0 }
        let spy = Spy()
        let b = body(10)
        let proj = await AIStrategies.getStrategy("truncate")!(
            b, [], .object([:]),
            ctx(1_000_000, summarize: { _ in
                spy.called += 1
                return "S"
            }))
        #expect(proj.omittedMarker == true && proj.summary == nil)
        #expect(proj.body[0] == b[0])  // first (task) turn kept
        #expect(Array(proj.body.suffix(4)) == Array(b.suffix(4)))  // recent kept
        #expect(!proj.body.contains { ($0["content"].asString ?? "").contains("[…") })  // no fabricated marker turn
        #expect(spy.called == 0, "truncate must NOT call the summarizer")
    }

    @Test func toolPairingNeverOrphansRoleTool() async {
        let b: [JSON] = [
            turn("user", "u0"),
            .object(["role": .string("assistant"), "content": .string("a"), "tool_calls": .array([.object(["id": .string("1")])])]),
            .object(["role": .string("tool"), "tool_call_id": .string("1"), "content": .string("r")]),
            turn("assistant", "done"),
            turn("user", "u-live"),
        ]
        // recent_n=3 would start the recent window on the role:tool (idx 2); must snap back.
        let proj = await AIStrategies.getStrategy("compact")!(b, [], .object([:]), ctx(1_000_000, recentN: 3))
        #expect(proj.body.first?["role"].asString != "tool")
    }

    @Test func summarizerFailureDegradesToStub() async {
        let proj = await AIStrategies.getStrategy("compact")!(
            body(10), [], .object([:]), ctx(1_000_000, summarize: { _ in throw MockError.boom }))
        #expect(proj.summary == AIStrategies.stubSummary)
    }

    @Test func tightBudgetKeepsLiveUserTurn() async {
        let b = body(10)
        let proj = await AIStrategies.getStrategy("truncate")!(b, [], .object([:]), ctx(80))
        #expect(proj.body.last == b.last)
    }

    // ── the projection seam (deterministic, with a booted kernel) ────────

    @Test func seamShortCircuitsWhenUnderBudget() async throws {
        let (kernel, agent, backend, sink, pump) = await seam()
        defer { pump.cancel() }
        let m = [turn("system", "S"), turn("user", "hi")]
        let out = await backend.projectContext(
            provider: MockProvider(), agent: agent, clientId: "cli", messages: m, storeLen: m.count - 1, kernel: kernel)
        guard case .projected(let got) = out else {
            Issue.record("must not failfast")
            return
        }
        #expect(got == m)  // unchanged
        #expect(backend.getProjection(AI) == .object(["fired": .bool(false)]))
        try? await Task.sleep(nanoseconds: 50_000_000)
        #expect(contextEvents(sink).isEmpty)  // no context event when nothing compacts
    }

    @Test func seamFiresPrependsNoticeAndEmitsEvent() async throws {
        let (kernel, agent, backend, sink, pump) = await seam(
            window: 500, meta: ["output_reserve": .integer(0), "context_strategy": .string("compact")])
        defer { pump.cancel() }
        let m = msgs(20)  // big history
        let out = await backend.projectContext(
            provider: MockProvider(tokens: ["FAKESUMMARY"]), agent: agent, clientId: "cli",
            messages: m, storeLen: m.count - 1, kernel: kernel)
        guard case .projected(let got) = out else {
            Issue.record("the projection must fire, not failfast")
            return
        }
        #expect(AIContext.estimateTokens(got) <= 500)  // fits the window now
        #expect(got[0]["role"].asString == "system")  // system block preserved
        #expect(got[1]["role"].asString == "user" && (got[1]["content"].asString ?? "").contains("[context-notice]"))
        let proj = backend.getProjection(AI)
        #expect(proj["fired"].asBool == true && proj["strategy"].asString == "compact")
        #expect(proj["summarized"].asBool == true && (proj["dropped_turns"].asInt ?? 0) > 0)
        // public projection record stays a clean summary — no internal scan bookkeeping
        #expect(proj.asObject?["fired_at_index"] == nil && proj.asObject?["client_id"] == nil)
        // the private reaction cursor holds the bookkeeping instead
        #expect(markOf(backend, AI) != nil)
        // the context:compacted event reached the caller
        try await waitUntil("the context event") { !contextEvents(sink).isEmpty }
        let events = contextEvents(sink)
        #expect(events.count == 1)
        let ev = events[0]
        #expect(ev["phase"].asString == "compacted" && ev["detail"]["strategy"].asString == "compact")
        #expect((ev["detail"]["dropped_turns"].asInt ?? 0) > 0)
    }

    @Test func seamNoticeIsNotInTheInputHistory() async throws {
        // The notice is a SEAM artifact — it must not appear in the conversation we
        // projected FROM (it is added only to the model view, never the durable record).
        let (kernel, agent, backend, _, pump) = await seam(
            window: 500, meta: ["output_reserve": .integer(0), "context_strategy": .string("compact")])
        defer { pump.cancel() }
        let m = msgs(20)
        #expect(!JSON.array(m).serialize().contains("[context-notice]"))
        _ = await backend.projectContext(
            provider: MockProvider(tokens: ["FAKESUMMARY"]), agent: agent, clientId: "web1",
            messages: m, storeLen: m.count - 1, kernel: kernel)
        // the input list itself was not mutated to contain the notice
        #expect(!JSON.array(m).serialize().contains("[context-notice]"))
    }

    @Test func seamFailsafeTooSmallEmitsEventAndErrors() async throws {
        let (kernel, agent, backend, sink, pump) = await seam(window: 1000)
        defer { pump.cancel() }
        let bigSystem = turn("system", String(repeating: "S", count: 8000))  // ~2000 tok, bigger than the window
        let m = [bigSystem, turn("user", "hi")]
        let out = await backend.projectContext(
            provider: MockProvider(), agent: agent, clientId: "cli", messages: m, storeLen: m.count - 1, kernel: kernel)
        guard case .failed(let err) = out else {
            Issue.record("the failsafe must refuse")
            return
        }
        #expect((err["error"].asString ?? "").contains("context_insufficient"))
        #expect(backend.getProjection(AI)["too_small"].asBool == true)
        try await waitUntil("the context event") { !contextEvents(sink).isEmpty }
        let ev = contextEvents(sink)
        #expect(ev.count == 1 && ev[0]["phase"].asString == "too_small")
        #expect(ev[0]["detail"]["hint"].asString != nil)
        // NAME THE KILLER — python's detail: the size and the head of the turn that did not fit.
        #expect(ev[0]["detail"]["last_turn_tokens"].asInt == AIContext.estimateOne(m[1]))
        #expect(ev[0]["detail"]["last_turn_head"].asString == "hi")
    }

    @Test func seamUnknownStrategyErrorsWithoutEvent() async throws {
        let (kernel, agent, backend, sink, pump) = await seam(
            window: 500, meta: ["output_reserve": .integer(0), "context_strategy": .string("bogus")])
        defer { pump.cancel() }
        let m = msgs(20)
        let out = await backend.projectContext(
            provider: MockProvider(), agent: agent, clientId: "cli", messages: m, storeLen: m.count - 1, kernel: kernel)
        guard case .failed(let err) = out else {
            Issue.record("an unknown strategy is a config error")
            return
        }
        #expect((err["error"].asString ?? "").contains("bogus"))
        // python's bytes: `{name}: unknown context_strategy {strat!r} (valid: {strategy_names()})`
        #expect(
            err["error"].asString
                == "mock_backend: unknown context_strategy 'bogus' (valid: ['compact', 'truncate', 'strip', 'fold'])")
        try? await Task.sleep(nanoseconds: 50_000_000)
        #expect(contextEvents(sink).isEmpty)
    }

    // ─── the GOAL must survive every strategy ───────────────────────
    //
    // MEASURED FAILURE (2026-08-20, python, live the model on the `live_plan`
    // probe, four runs). The task is stated ONCE, in the first user turn. On a
    // long run the body overflows and `compact` — the DEFAULT — splits at
    // `recent_n` and replaces the whole overflow with an LLM summary. The oldest
    // turn IS the task framing, so the goal is the FIRST thing at risk. Observed
    // end state: the model had already worked out the correct answer, then
    // finished with "healthy budget, no tasks" and "Idle and ready. Give me a task
    // whenever." — it did not refuse to act, it no longer knew what it was doing.
    //
    // `truncate` already gets this right, which is what made the diagnosis
    // obvious. This test is deliberately GENERAL over the strategy registry: a new
    // strategy inherits the guarantee by being registered.
    @Test func everyStrategyKeepsTheGoalWhenTheBodyOverflows() async {
        let goal = "GOAL-SENTINEL: seal the secrets bridge and open the assets bridge."
        let b = [turn("user", goal)] + body(40, size: 400)
        for name in AIStrategies.strategyNames() {
            let strategy = AIStrategies.getStrategy(name)!
            let proj = await strategy(b, [], .object([:]), ctx(1200, selfId: "\(AI)_\(name)"))
            // The projection is what the model actually sees. The goal must be
            // reachable from it — carried verbatim in the body, or preserved in the
            // summary the seam turns into the context notice.
            let carried = proj.body.contains { ($0["content"].asString ?? "").contains(goal) }
            let summarized = proj.summary?.contains(goal) ?? false
            #expect(
                carried || summarized,
                "strategy '\(name)' dropped the task-framing turn: the agent loses its goal mid-run (body=\(proj.body.count) turns, summary=\(String(describing: proj.summary)))"
            )
        }
    }

    // ── the SEAM must not undo what the strategies pinned ────────────────

    /// `everyStrategyKeepsTheGoalWhenTheBodyOverflows` calls the strategies
    /// DIRECTLY, so it never saw the layer that undid them.
    ///
    /// `projectContext` appends the real notice and then trims to fit. It trimmed
    /// from the FRONT — exactly where `compact`/`truncate` had just pinned the
    /// goal turn — so a snug window silently dropped the one turn the whole run
    /// serves. Measured 2026-08-28 (python): the goal sentence was absent from the
    /// projection across a wide band of windows, in BOTH strategies. The cause was
    /// a notice envelope reserved at 80 tokens against a real ~132, which put the
    /// projection over budget and woke the backstop.
    ///
    /// This asserts through the seam, which is the only place the bug was visible.
    @Test func theSeamKeepsTheGoalTheStrategiesPinned() async throws {
        let goal = "GOAL-SENTINEL: build the widget and report when done"
        for strategy in AIStrategies.strategyNames().sorted() {
            for window: Int64 in [300, 400, 500, 700, 900, 1200] {
                let (kernel, agent, backend, _, pump) = await seam(
                    window: window,
                    meta: ["output_reserve": .integer(0), "context_strategy": .string(strategy)])
                defer { pump.cancel() }
                let m = [turn("system", "sys"), turn("user", goal)] + body(40)
                let out = await backend.projectContext(
                    provider: MockProvider(tokens: ["FAKESUMMARY"]), agent: agent, clientId: "cli",
                    messages: m, storeLen: m.count - 1, kernel: kernel)
                guard case .projected(let got) = out else {
                    Issue.record("\(strategy)@\(window): the projection must fire, not failfast")
                    continue
                }
                #expect(AIContext.estimateTokens(got) <= window, "the projection must still fit")
                let kept = got.contains { ($0["content"].asString ?? "").contains(goal) }
                #expect(
                    kept,
                    "\(strategy)@\(window): the pinned goal turn was dropped by the seam — kept \(got.map { $0["role"].asString ?? "" })"
                )
            }
        }
    }
}
