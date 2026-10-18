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
    ProjectionCtx(
        budget: bud, recentN: recentN, summarize: summarize.map(summarizer), selfId: AgentId(selfId), kernel: nil,
        anchors: [:])  // these bodies hold no fold rows
}

/// python `_FakeKernel` + the record: a booted kernel whose `ai_c` agent carries
/// the given context meta, a backend to drive the seam, and a sink on the
/// agent's inbox for the `context` events.
private func seam(
    window: Int64 = 32768, meta: OrderedDictionary<String, JSON> = ["output_reserve": .integer(0)]
) async -> (Kernel, Agent, AIBackend, EventSink, Task<Void, Never>) {
    let config = baseConfig { MockProvider(tokens: ["FAKESUMMARY"]) }
    let (kernel, ids, _) = await bootMockKernel(config: config, backendIds: [AI], contextWindow: window, meta: meta)
    let agent = kernel.agent(ids[AI]!)!
    let sink = EventSink()
    // THE CALLER IS `cli`, so the seam's context events are SENDS to the cli
    // agent — python's twin asserts exactly that (`k.sent`, "cli → kernel.send").
    // The cli stand-in forwards what it receives into the sink; the backend's own
    // inbox is pumped too, so an event that went the WRONG way is seen as well.
    kernel.bundles.register("sink_cli.tools", SinkBundle(sink))
    _ = await kernel.send([
        Item(
            AgentId("kernel_state"),
            .object([
                "type": .string("create_agent"), "parent_id": .string("kernel_state"),
                "handler_module": .string("sink_cli.tools"), "id": .string("cli"),
                "callable_by": .object([AI: .string("*")]),
            ]))
    ])[0].result
    let inbox = kernel.ensureInbox(agent.id)
    let pump = Task { for await ev in inbox { sink.add(ev.payload) } }
    return (kernel, agent, buildAIBackend(config), sink, pump)
}

/// A `cli` stand-in: every payload it is sent lands in `sink`.
private final class SinkBundle: AgentBundle, @unchecked Sendable {
    let name = "sink_cli"
    let sink: EventSink
    init(_ sink: EventSink) { self.sink = sink }
    func handle(agentId: AgentId, payload: JSON, kernel: Kernel) async throws -> JSON? {
        switch payload["type"].asString ?? "" {
        case "boot", "shutdown", "reflect": return nil
        default:
            sink.add(payload)
            return nil
        }
    }
}

/// A meta-only agent, for the record-reading primitives.
private func rec(_ meta: OrderedDictionary<String, JSON>) -> Agent {
    Agent(id: AgentId("rec"), handlerModule: nil, parentId: nil, meta: meta)
}

@Suite("the context seam", .serialized)
struct ContextSeamTests {
    init() { AIStripStrategy.forget(nil, AI) }

    // ── primitives ──────────────────────────────────────────────────────

    @Test func estimateScaleAndMonotonic() {
        #expect(AIContext.estimateOne(turn("user", String(repeating: "x", count: 400))) >= 100)  // ~400/4
        #expect(AIContext.estimateTokens(body(4)) > AIContext.estimateTokens(body(2)))
    }

    @Test func windowPrecedence() {
        #expect(AIContext.resolveContextWindow(rec(["context_window": .integer(8000), "num_ctx": .integer(2000)]), modelWindow: AIConstants.DEFAULT_CONTEXT_WINDOW) == 8000)
        #expect(AIContext.resolveContextWindow(rec(["num_ctx": .integer(2000)]), modelWindow: AIConstants.DEFAULT_CONTEXT_WINDOW) == 2000)
        #expect(AIContext.resolveContextWindow(rec([:]), modelWindow: AIConstants.DEFAULT_CONTEXT_WINDOW) == AIConstants.DEFAULT_CONTEXT_WINDOW)
    }

    @Test func budgetFloor() {
        #expect(AIContext.budget(rec(["context_window": .integer(100), "output_reserve": .integer(1000)]), modelWindow: AIConstants.DEFAULT_CONTEXT_WINDOW) == 256)
    }

    @Test func registryNoMemgpt() {
        // memgpt is not a strategy: its nudge lives in the universal seam notice.
        #expect(Set(AIStrategies.strategyNames()) == ["compact", "truncate", "strip", "fold"])
        #expect(AIStrategies.defaultStrategy == "compact")
        #expect(AIStrategies.getStrategy("memgpt") == nil)
        #expect(AIStrategies.getStrategy("nope") == nil)  // unknown ⇒ nil (caller errors), no default
    }

    // ── strategies (return a Projection, NOT a fabricated turn) ─────────

    @Test func compactReturnsProjectionWithSummary() async throws {
        let b = body(10)
        let proj = try await AIStrategies.getStrategy("compact")!(b, [turn("system", "S")], .object([:]), ctx(1_000_000))
        #expect(proj.summary == "SUMMARY")  // artifact, not a fabricated turn
        // The task-framing turn is PINNED ahead of the recent window (see
        // `everyStrategyKeepsTheGoalWhenTheBodyOverflows` for why). Everything after
        // it is the recent window, verbatim.
        #expect(proj.body == [b[0]] + b.suffix(4))
        // the strategy never fabricates a leading marker turn
        #expect(!proj.body.contains { ($0["content"].asString ?? "").contains("[Earlier conversation summary]") })
    }

    @Test func truncateReturnsProjectionNoSummarizer() async throws {
        final class Spy: @unchecked Sendable { var called = 0 }
        let spy = Spy()
        let b = body(10)
        let proj = try await AIStrategies.getStrategy("truncate")!(
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

    @Test func summarizerFailureDegradesToStub() async throws {
        let proj = try await AIStrategies.getStrategy("compact")!(
            body(10), [], .object([:]), ctx(1_000_000, summarize: { _ in throw MockError.boom }))
        #expect(proj.summary == AIStrategies.stubSummary)
    }

    @Test func tightBudgetKeepsLiveUserTurn() async throws {
        let b = body(10)
        let proj = try await AIStrategies.getStrategy("truncate")!(b, [], .object([:]), ctx(80))
        #expect(proj.body.last == b.last)
    }

    // ── the projection seam (deterministic, with a booted kernel) ────────

    @Test func seamShortCircuitsWhenUnderBudget() async throws {
        let (kernel, agent, backend, sink, pump) = await seam()
        defer { pump.cancel() }
        let m = [turn("system", "S"), turn("user", "hi")]
        let out = try await backend.projectContext(
            provider: MockProvider(), agent: agent, clientId: "cli", messages: m, storeLen: m.count - 1, kernel: kernel)
        guard case .projected(let got) = out else {
            Issue.record("must not failfast")
            return
        }
        #expect(got == m)  // unchanged
        // python's `{fired: False, trigger, calibration}` — the calibrated trigger
        // it was measured against rides the posture.
        #expect(
            backend.getProjection(AI)
                == .object([
                    "fired": .bool(false), "trigger": .double(0.8), "calibration": .double(1.0),
                ]))
        try? await Task.sleep(nanoseconds: 50_000_000)
        #expect(contextEvents(sink).isEmpty)  // no context event when nothing compacts
    }

    @Test func seamFiresPrependsNoticeAndEmitsEvent() async throws {
        let (kernel, agent, backend, sink, pump) = await seam(
            window: 500, meta: ["output_reserve": .integer(0), "context_strategy": .string("compact")])
        defer { pump.cancel() }
        let m = msgs(20)  // big history
        let out = try await backend.projectContext(
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
        #expect(proj["summarized"].asBool == true && (proj["dropped_messages"].asInt ?? 0) > 0)
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
        #expect((ev["detail"]["dropped_messages"].asInt ?? 0) > 0)
    }

    @Test func seamNoticeIsNotInTheInputHistory() async throws {
        // The notice is a SEAM artifact — it must not appear in the conversation we
        // projected FROM (it is added only to the model view, never the durable record).
        let (kernel, agent, backend, _, pump) = await seam(
            window: 500, meta: ["output_reserve": .integer(0), "context_strategy": .string("compact")])
        defer { pump.cancel() }
        let m = msgs(20)
        #expect(!JSON.array(m).serialize().contains("[context-notice]"))
        _ = try await backend.projectContext(
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
        let out = try await backend.projectContext(
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
        #expect(ev[0]["detail"]["last_message_tokens"].asInt == AIContext.estimateOne(m[1]))
        #expect(ev[0]["detail"]["last_message_head"].asString == "hi")
    }

    @Test func seamUnknownStrategyErrorsWithoutEvent() async throws {
        let (kernel, agent, backend, sink, pump) = await seam(
            window: 500, meta: ["output_reserve": .integer(0), "context_strategy": .string("bogus")])
        defer { pump.cancel() }
        let m = msgs(20)
        let out = try await backend.projectContext(
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
    // The task is stated ONCE, in the first user turn. On a long run the body
    // overflows and `compact` — the DEFAULT — splits at `recent_n` and replaces the
    // whole overflow with an LLM summary. The oldest turn IS the task framing, so
    // the goal is the FIRST thing at risk: a model that loses it no longer knows
    // what it is doing.
    //
    // This test is deliberately GENERAL over the strategy registry: a new
    // strategy inherits the guarantee by being registered.
    @Test func everyStrategyKeepsTheGoalWhenTheBodyOverflows() async throws {
        let goal = "GOAL-SENTINEL: seal the secrets bridge and open the assets bridge."
        let b = [turn("user", goal)] + body(40, size: 400)
        for name in AIStrategies.strategyNames() {
            let strategy = AIStrategies.getStrategy(name)!
            let proj = try await strategy(b, [], .object([:]), ctx(1200, selfId: "\(AI)_\(name)"))
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
    /// DIRECTLY, so it never sees the layer after them.
    ///
    /// `projectContext` appends the real notice and then trims to fit. Trimming
    /// from the FRONT — exactly where `compact`/`truncate` just pinned the goal
    /// turn — would silently drop the one turn the whole run serves.
    ///
    /// This asserts through the seam, the only place such a drop is visible.
    @Test func theSeamKeepsTheGoalTheStrategiesPinned() async throws {
        let goal = "GOAL-SENTINEL: build the widget and report when done"
        for strategy in AIStrategies.strategyNames().sorted() {
            for window: Int64 in [300, 400, 500, 700, 900, 1200] {
                let (kernel, agent, backend, _, pump) = await seam(
                    window: window,
                    meta: ["output_reserve": .integer(0), "context_strategy": .string(strategy)])
                defer { pump.cancel() }
                let m = [turn("system", "sys"), turn("user", goal)] + body(40)
                let out = try await backend.projectContext(
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

/// python `_as_id` / `_as_pos_int`: a digit string counts only when it is
/// ASCII (`s.isascii() and s.isdigit()`) — `'٣'` and `'²'` are not ids.
@Suite("ai_core ids are ASCII digit strings")
struct AIAsIdTests {
    @Test func onlyAsciiDigitStringsAreIds() {
        #expect(aiAsId(.string(" 12 ")) == 12)
        #expect(aiAsId(.string("0")) == 0)
        #expect(aiAsId(.string("٣")) == nil)
        #expect(aiAsId(.string("²")) == nil)
        #expect(aiAsId(.string("+5")) == nil)
        #expect(!AIContext.pyDigits("١٢") && AIContext.pyDigits("12"))
    }

    /// `context_window` / `output_reserve` parse the STRIPPED text they check:
    /// python's strip drops `\x1c`–`\x1f`, `int()` does not, so parsing the raw
    /// value raised on `"\x1c12"`. Same vectors as python `test_context.py` and
    /// rust `id_and_window_digit_strings_are_ascii_only`.
    @Test func windowAndReserveParseTheStrippedText() {
        let w = AIConstants.DEFAULT_CONTEXT_WINDOW
        let r = AIConstants.DEFAULT_OUTPUT_RESERVE
        let vectors: [(String, Int64, Int64)] = [
            ("٣", w, r), ("²", w, r), ("\u{2003} 12 ", 12, 12), ("\u{1c}12", 12, 12),
            ("\u{1f}12\u{1d}", 12, 12), ("0", w, 0), ("\u{1c}", w, r), ("+5", w, r),
        ]
        for (v, window, reserve) in vectors {
            let a = rec(["context_window": .string(v), "output_reserve": .string(v)])
            #expect(AIContext.resolveContextWindow(a, modelWindow: w) == window, "\(v.debugDescription)")
            #expect(AIContext.outputReserve(a) == reserve, "\(v.debugDescription)")
        }
    }
}
