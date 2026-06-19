// Shared AI-backend machinery tests — driven through a MOCK
// `AIProvider` so the verb surface, the spawned streaming loop,
// history persistence, tool-call persistence, cancellation, and the
// stateless/error-shape config flags are all exercised WITHOUT any
// live LLM. These cover the behaviour the three real backends used to
// each test separately; provider-specific wire tests stay with each
// backend target.

import FantasticAICore
import FantasticJSON
import FantasticKernel
import Foundation
import Testing

// MARK: - Mock provider + bundle

/// Deterministic provider replaying a SCRIPT of passes — one per
/// `chat()` call (the agentic loop calls `chat` once per turn). Each
/// pass is a sequence of chunks (tokens + finalized tool-calls). Once
/// the script is exhausted, `chat` yields an empty pass, which the loop
/// reads as "no more tools" and terminates. `chat` can also be told to
/// throw. Mirrors the Rust `MockProvider` (scripted passes).
private final class MockProvider: AIProvider, @unchecked Sendable {
    let model: String
    private let lock = NSLock()
    private var passes: [[AIChunk]]
    let throwsError: Bool
    /// Per-chunk delay (ns) so a generation can be observed in-flight.
    let chunkDelayNanos: UInt64

    /// Single-pass convenience: tokens only, no tool-calls.
    init(
        model: String = "mock-1", tokens: [String] = ["Hel", "lo"],
        throwsError: Bool = false, chunkDelayNanos: UInt64 = 0
    ) {
        self.model = model
        self.passes = [tokens.map { .token($0) }]
        self.throwsError = throwsError
        self.chunkDelayNanos = chunkDelayNanos
    }

    /// Multi-pass: drive the agentic loop with scripted tool-calls.
    init(model: String = "mock-1", passes: [[AIChunk]], chunkDelayNanos: UInt64 = 0) {
        self.model = model
        self.passes = passes
        self.throwsError = false
        self.chunkDelayNanos = chunkDelayNanos
    }

    func chat(messages: [JSON], tools: [JSON]) -> AsyncThrowingStream<AIChunk, Error> {
        let throwsError = throwsError
        let chunkDelayNanos = chunkDelayNanos
        lock.lock()
        let pass = passes.isEmpty ? [] : passes.removeFirst()
        lock.unlock()
        return AsyncThrowingStream { continuation in
            Task {
                if throwsError {
                    continuation.finish(throwing: MockError.boom)
                    return
                }
                for c in pass {
                    if chunkDelayNanos > 0 { try? await Task.sleep(nanoseconds: chunkDelayNanos) }
                    continuation.yield(c)
                }
                continuation.finish()
            }
        }
    }
}

/// Build a finalized `send` tool-call chunk in the OpenAI shape the
/// shared loop dispatches.
private func sendCall(id: String, target: String, verb: String) -> AIChunk {
    .toolCall(
        .object([
            "id": .string(id),
            "type": .string("function"),
            "function": .object([
                "name": .string("send"),
                "arguments": .string(
                    "{\"target_id\":\"\(target)\",\"payload\":{\"type\":\"\(verb)\"}}"),
            ]),
        ]))
}

private enum MockError: Error, CustomStringConvertible {
    case boom
    var description: String { "boom" }
}

/// Test bundle wrapping `AIBackend` with a configurable mock provider.
private final class MockBundle: AgentBundle, @unchecked Sendable {
    let name = "mock_backend"
    private let core: AIBackend

    init(config: AIBackendConfig) {
        self.core = buildAIBackend(config)
    }

    func handle(agentId: AgentId, payload: JSON, kernel: Kernel) async throws -> JSON? {
        await core.handle(agentId: agentId, payload: payload, kernel: kernel)
    }
}

/// In-memory file_bridge stand-in: dict-backed `read`/`write` so the AI
/// backend's real `file_bridge_id` persistence path is exercised without
/// touching disk (a real file_bridge is integration-tested elsewhere).
private final class MockFileBridge: AgentBundle, @unchecked Sendable {
    let name = "mock_file_bridge"
    private let lock = NSLock()
    private var files: [String: String] = [:]

    private func get(_ path: String) -> String? {
        lock.lock()
        defer { lock.unlock() }
        return files[path]
    }
    private func put(_ path: String, _ content: String) {
        lock.lock()
        defer { lock.unlock() }
        files[path] = content
    }

    func handle(agentId: AgentId, payload: JSON, kernel: Kernel) async throws -> JSON? {
        switch payload["type"].asString ?? "" {
        case "reflect":
            return .object([
                "id": .string(agentId.value), "sentence": .string("mock fs"),
                "kind": .string("file_bridge"), "verbs": .object([:]),
            ])
        case "read":
            let path = payload["path"].asString ?? ""
            return get(path).map { .object(["content": .string($0)]) }
                ?? .object(["error": .string("not found")])
        case "write":
            let path = payload["path"].asString ?? ""
            let content = payload["content"].asString ?? ""
            put(path, content)
            return .object(["written": .integer(Int64(content.utf8.count))])
        default:
            return .object(["error": .string("unknown verb")])
        }
    }
}

private func baseConfig(
    stateless: Bool = false,
    persistToolCalls: Bool = false,
    includeAccumulatedOnError: Bool = false,
    emitInterruptedError: Bool = false,
    provider: @escaping @Sendable () -> ProviderResult
) -> AIBackendConfig {
    AIBackendConfig(
        kind: "mock_backend",
        provider: "mock",
        sentence: "Mock LLM agent.",
        verbs: [
            "send": "s", "history": "h", "interrupt": "i", "backend_state": "b",
        ] as JSON,
        stateless: stateless,
        persistToolCalls: persistToolCalls,
        includeAccumulatedOnError: includeAccumulatedOnError,
        emitInterruptedError: emitInterruptedError,
        reflectExtra: { _ in ["extra": .string("yes")] },
        backendStateExtra: { _ in ["configured": .bool(true)] },
        makeProvider: { _, _, _ in provider() }
    )
}

/// Boot an in-memory kernel whose registry has the mock backend, then
/// create an agent for it. Built directly (not via
/// `startKernelInMemory`) so we can inject the mock bundle.
private func freshMockKernel(config: AIBackendConfig) async throws -> (Kernel, AgentId) {
    let registry = BundleRegistry()
    registry.register("mock_backend.tools", MockBundle(config: config))
    registry.register("mock_file_bridge.tools", MockFileBridge())
    let kernel = Kernel(storage: .inMemory, bundles: registry)
    let root = Agent(id: AgentId("core"), handlerModule: nil, parentId: nil)
    kernel.register(root)
    kernel.setRoot(root)
    // A file_bridge store so chat history persists through file_bridge_id.
    _ = await kernel.send(
        AgentId("core"),
        .object([
            "type": .string("create_agent"),
            "handler_module": .string("mock_file_bridge.tools"),
            "id": .string("store"),
        ]))
    let r = await kernel.send(
        AgentId("core"),
        .object([
            "type": .string("create_agent"),
            "handler_module": .string("mock_backend.tools"),
            "id": .string("mock"),
            "file_bridge_id": .string("store"),
        ]))
    return (kernel, AgentId(r["id"].asString ?? "mock"))
}

// MARK: - Tests

@Suite("AIBackend shared machinery")
struct AIBackendTests {

    @Test func reflectMergesExtraAndKeepsOrder() async throws {
        let (kernel, id) = try await freshMockKernel(
            config: baseConfig { .provider(MockProvider()) })
        let r = await kernel.send(id, .object(["type": .string("reflect")]))
        #expect(r["id"].asString == id.value)
        #expect(r["kind"].asString == "mock_backend")
        #expect(r["provider"].asString == "mock")
        #expect(r["extra"].asString == "yes")
        #expect(r["verbs"]["send"].asString == "s")
    }

    @Test func backendStateMergesExtra() async throws {
        let (kernel, id) = try await freshMockKernel(
            config: baseConfig { .provider(MockProvider()) })
        let r = await kernel.send(id, .object(["type": .string("backend_state")]))
        #expect(r["provider"].asString == "mock")
        #expect(r["configured"].asBool == true)
    }

    @Test func unknownVerbErrors() async throws {
        let (kernel, id) = try await freshMockKernel(
            config: baseConfig { .provider(MockProvider()) })
        let r = await kernel.send(id, .object(["type": .string("nope")]))
        #expect(r["error"].asString == "unknown verb nope")
    }

    @Test func sendReturnsQueuedShape() async throws {
        let (kernel, id) = try await freshMockKernel(
            config: baseConfig { .provider(MockProvider()) })
        let r = await kernel.send(
            id, .object(["type": .string("send"), "text": .string("hi")]))
        #expect(r["queued"].asBool == true)
        #expect(r["stream_id"].asString?.hasPrefix("stm_") == true)
        #expect(r["message_id"].asString?.hasPrefix("msg_") == true)
        #expect(r["client_id"].asString == "cli")
    }

    @Test func sendWithoutTextErrors() async throws {
        let (kernel, id) = try await freshMockKernel(
            config: baseConfig { .provider(MockProvider()) })
        let r = await kernel.send(id, .object(["type": .string("send")]))
        #expect(r["error"].asString == "send requires text")
    }

    @Test func makeProviderRefusalReturnedVerbatim() async throws {
        let refusal: JSON = .object([
            "error": .string("no_key"), "reason": .string("missing"),
        ])
        let (kernel, id) = try await freshMockKernel(
            config: baseConfig { .refused(refusal) })
        let r = await kernel.send(
            id, .object(["type": .string("send"), "text": .string("hi")]))
        #expect(r["error"].asString == "no_key")
        #expect(r["reason"].asString == "missing")
        #expect(r["queued"].asBool == nil)
    }

    @Test func streamPersistsUserAndAssistantHistory() async throws {
        let (kernel, id) = try await freshMockKernel(
            config: baseConfig { .provider(MockProvider(tokens: ["Hel", "lo"])) })
        _ = await kernel.send(
            id,
            .object([
                "type": .string("send"), "text": .string("hi"),
                "client_id": .string("c1"),
            ]))
        try await waitForHistory(kernel: kernel, id: id, client: "c1", count: 2)

        let h = await kernel.send(
            id, .object(["type": .string("history"), "client_id": .string("c1")]))
        let msgs = h["messages"].asArray ?? []
        #expect(msgs.count == 2)
        #expect(msgs[0]["role"].asString == "user")
        #expect(msgs[0]["content"].asString == "hi")
        #expect(msgs[0]["complete"].asBool == true)
        #expect(msgs[0]["id"].asString == nil)  // non-stateless: no id
        #expect(msgs[1]["role"].asString == "assistant")
        #expect(msgs[1]["content"].asString == "Hello")
        #expect(msgs[1]["tool_calls"].asArray == nil)
    }

    @Test func statelessRowsCarryIdAndDoNotFeedHistoryBack() async throws {
        let (kernel, id) = try await freshMockKernel(
            config: baseConfig(stateless: true) { .provider(MockProvider(tokens: ["x"])) })
        _ = await kernel.send(
            id,
            .object([
                "type": .string("send"), "text": .string("one"),
                "client_id": .string("c"),
            ]))
        try await waitForHistory(kernel: kernel, id: id, client: "c", count: 2)
        let h = await kernel.send(
            id, .object(["type": .string("history"), "client_id": .string("c")]))
        let msgs = h["messages"].asArray ?? []
        #expect(msgs[0]["id"].asString != nil)  // stateless: id present
        #expect(msgs[1]["id"].asString != nil)
    }

    @Test func agenticLoopExecutesToolCallThenFinishes() async throws {
        // Pass 1: a `send` tool-call (reflect `core`). Pass 2: the final
        // answer. The loop must dispatch the call through the kernel,
        // feed the reply back, and stop when no more tools come.
        let (kernel, id) = try await freshMockKernel(
            config: baseConfig {
                .provider(
                    MockProvider(passes: [
                        [.token("checking…"), sendCall(id: "a", target: "core", verb: "reflect")],
                        [.token("all set")],
                    ]))
            })
        _ = await kernel.send(
            id,
            .object([
                "type": .string("send"), "text": .string("go"),
                "client_id": .string("c"),
            ]))
        // user, assistant(tool_calls), tool result, final assistant.
        try await waitForHistory(kernel: kernel, id: id, client: "c", count: 4)
        let msgs =
            (await kernel.send(
                id, .object(["type": .string("history"), "client_id": .string("c")])))[
                "messages"
            ].asArray ?? []
        #expect(msgs.count == 4)
        #expect(msgs[0]["role"].asString == "user")
        // The assistant turn that triggered the tool carries its call.
        #expect(msgs[1]["role"].asString == "assistant")
        let tc = msgs[1]["tool_calls"].asArray ?? []
        #expect(tc.count == 1)
        #expect(tc[0]["id"].asString == "a")
        // The kernel reply rides back as a role:tool message.
        #expect(msgs[2]["role"].asString == "tool")
        #expect(msgs[2]["tool_call_id"].asString == "a")
        #expect(msgs[2]["content"].asString?.contains("core") == true)
        // The final no-tool pass is the answer.
        #expect(msgs[3]["role"].asString == "assistant")
        #expect(msgs[3]["content"].asString == "all set")
        #expect(msgs[3]["tool_calls"].asArray == nil)
    }

    @Test func toolCallWithEmptyTargetErrorsButLoopContinues() async throws {
        // A malformed call (no target_id) must not wedge the loop: the
        // dispatch returns an error reply, the loop feeds it back and the
        // next pass concludes.
        let badCall: AIChunk = .toolCall(
            .object([
                "id": .string("z"),
                "function": .object([
                    "name": .string("send"),
                    "arguments": .string("{\"payload\":{\"type\":\"reflect\"}}"),
                ]),
            ]))
        let (kernel, id) = try await freshMockKernel(
            config: baseConfig {
                .provider(MockProvider(passes: [[badCall], [.token("ok")]]))
            })
        _ = await kernel.send(
            id,
            .object([
                "type": .string("send"), "text": .string("go"),
                "client_id": .string("c"),
            ]))
        try await waitForHistory(kernel: kernel, id: id, client: "c", count: 4)
        let msgs =
            (await kernel.send(
                id, .object(["type": .string("history"), "client_id": .string("c")])))[
                "messages"
            ].asArray ?? []
        #expect(msgs[2]["role"].asString == "tool")
        #expect(msgs[2]["content"].asString?.contains("empty target_id") == true)
        #expect(msgs[3]["content"].asString == "ok")
    }

    @Test func errorStreamDoesNotPersistAssistantTurn() async throws {
        let (kernel, id) = try await freshMockKernel(
            config: baseConfig { .provider(MockProvider(throwsError: true)) })
        _ = await kernel.send(
            id,
            .object([
                "type": .string("send"), "text": .string("hi"),
                "client_id": .string("c"),
            ]))
        // Only the user turn persists on error; give the stream a beat.
        try await waitForHistory(kernel: kernel, id: id, client: "c", count: 1)
        let h = await kernel.send(
            id, .object(["type": .string("history"), "client_id": .string("c")]))
        let msgs = h["messages"].asArray ?? []
        #expect(msgs.count == 1)
        #expect(msgs[0]["role"].asString == "user")
    }

    @Test func interruptReturnsInterrupted() async throws {
        let (kernel, id) = try await freshMockKernel(
            config: baseConfig { .provider(MockProvider()) })
        let r = await kernel.send(id, .object(["type": .string("interrupt")]))
        #expect(r["interrupted"].asBool == true)
    }

    @Test func historyIsPerClient() async throws {
        let (kernel, id) = try await freshMockKernel(
            config: baseConfig { .provider(MockProvider(tokens: ["z"])) })
        _ = await kernel.send(
            id,
            .object([
                "type": .string("send"), "text": .string("a"),
                "client_id": .string("c1"),
            ]))
        try await waitForHistory(kernel: kernel, id: id, client: "c1", count: 2)
        let h2 = await kernel.send(
            id, .object(["type": .string("history"), "client_id": .string("c2")]))
        #expect((h2["messages"].asArray ?? []).isEmpty)
        #expect(h2["client_id"].asString == "c2")
    }

    @Test func historyPersistsAcrossTurnsThroughFileBridge() async throws {
        // Two sends on one client: turn 2's persisted history must carry
        // turn 1 (the load→save round-trip through file_bridge_id).
        let (kernel, id) = try await freshMockKernel(
            config: baseConfig { .provider(MockProvider(tokens: ["x"])) })
        _ = await kernel.send(
            id,
            .object([
                "type": .string("send"), "text": .string("one"),
                "client_id": .string("c"),
            ]))
        try await waitForHistory(kernel: kernel, id: id, client: "c", count: 2)
        _ = await kernel.send(
            id,
            .object([
                "type": .string("send"), "text": .string("two"),
                "client_id": .string("c"),
            ]))
        try await waitForHistory(kernel: kernel, id: id, client: "c", count: 4)
        let msgs =
            (await kernel.send(
                id, .object(["type": .string("history"), "client_id": .string("c")])))[
                "messages"
            ].asArray ?? []
        #expect(msgs.count == 4)
        #expect(msgs[0]["content"].asString == "one")
        #expect(msgs[1]["role"].asString == "assistant")
        #expect(msgs[2]["content"].asString == "two")
        #expect(msgs[3]["role"].asString == "assistant")
    }

    @Test func historyStaysEmptyWithoutFileBridge() async throws {
        // No file_bridge_id wired ⇒ no persistence (RAM-empty), NOT a
        // silent fallback. The send still streams; history just doesn't
        // accumulate.
        let registry = BundleRegistry()
        registry.register(
            "mock_backend.tools", MockBundle(config: baseConfig { .provider(MockProvider()) }))
        let kernel = Kernel(storage: .inMemory, bundles: registry)
        let root = Agent(id: AgentId("core"), handlerModule: nil, parentId: nil)
        kernel.register(root)
        kernel.setRoot(root)
        let r = await kernel.send(
            AgentId("core"),
            .object([
                "type": .string("create_agent"),
                "handler_module": .string("mock_backend.tools"), "id": .string("naked"),
            ]))
        let id = AgentId(r["id"].asString ?? "naked")
        _ = await kernel.send(
            id, .object(["type": .string("send"), "text": .string("hi")]))
        // Give the stream a beat, then confirm history is still empty.
        try? await Task.sleep(nanoseconds: 60_000_000)
        let h = await kernel.send(id, .object(["type": .string("history")]))
        #expect((h["messages"].asArray ?? []).isEmpty)
    }

    @Test func statusIdleShapeWhenNothingInFlight() async throws {
        let (kernel, id) = try await freshMockKernel(
            config: baseConfig { .provider(MockProvider()) })
        let s = await kernel.send(
            id, .object(["type": .string("status"), "client_id": .string("c")]))
        #expect(s["source"].asString == id.value)
        #expect(s["generating"].asBool == false)
        #expect(s["current"].isNull)
        #expect((s["mine_pending"].asArray ?? []).isEmpty)
        #expect(s["others_pending"].asInt == 0)
    }

    @Test func statusReflectsInFlightForOwner() async throws {
        // A slow stream stays in-flight long enough to observe `status`.
        let (kernel, id) = try await freshMockKernel(
            config: baseConfig {
                .provider(
                    MockProvider(tokens: ["a", "b", "c"], chunkDelayNanos: 40_000_000))
            })
        _ = await kernel.send(
            id,
            .object([
                "type": .string("send"), "text": .string("hi there"),
                "client_id": .string("c"),
            ]))
        // Poll until the generation is in-flight.
        var snap: JSON = .null
        for _ in 0..<50 {
            let s = await kernel.send(
                id, .object(["type": .string("status"), "client_id": .string("c")]))
            if s["generating"].asBool == true {
                snap = s
                break
            }
            try await Task.sleep(nanoseconds: 5_000_000)
        }
        #expect(snap["generating"].asBool == true)
        let cur = snap["current"]
        #expect(cur["is_mine"].asBool == true)
        #expect(cur["text"].asString == "hi there")  // owner sees own text
        #expect(["thinking", "streaming", "tool_calling"].contains(cur["phase"].asString ?? ""))
    }

    @Test func statusRedactsOtherClientsText() async throws {
        let (kernel, id) = try await freshMockKernel(
            config: baseConfig {
                .provider(MockProvider(tokens: ["a", "b"], chunkDelayNanos: 40_000_000))
            })
        _ = await kernel.send(
            id,
            .object([
                "type": .string("send"), "text": .string("secret"),
                "client_id": .string("owner"),
            ]))
        // A DIFFERENT client polls — must not see the owner's text.
        var cur: JSON = .null
        for _ in 0..<50 {
            let s = await kernel.send(
                id, .object(["type": .string("status"), "client_id": .string("nosy")]))
            if s["generating"].asBool == true {
                cur = s["current"]
                break
            }
            try await Task.sleep(nanoseconds: 5_000_000)
        }
        #expect(cur["is_mine"].asBool == false)
        #expect(cur["text"].asString == nil)  // redacted
        #expect(cur["phase"].asString != nil)  // phase still visible
    }

    @Test func fifoSerializesConcurrentSendsKeepingHistoryIntact() async throws {
        // Two overlapping sends on one client. FIFO serialization keeps
        // the load→save of chat history race-free: both turns survive.
        let (kernel, id) = try await freshMockKernel(
            config: baseConfig {
                .provider(MockProvider(tokens: ["x"], chunkDelayNanos: 20_000_000))
            })
        async let a: JSON = kernel.send(
            id,
            .object([
                "type": .string("send"), "text": .string("one"), "client_id": .string("c"),
            ]))
        async let b: JSON = kernel.send(
            id,
            .object([
                "type": .string("send"), "text": .string("two"), "client_id": .string("c"),
            ]))
        _ = await (a, b)
        // Both generations: 2 turns each (user + assistant) = 4 rows.
        try await waitForHistory(kernel: kernel, id: id, client: "c", count: 4)
        let msgs =
            (await kernel.send(
                id, .object(["type": .string("history"), "client_id": .string("c")])))[
                "messages"
            ].asArray ?? []
        #expect(msgs.count == 4)
        let texts = msgs.filter { $0["role"].asString == "user" }.compactMap {
            $0["content"].asString
        }
        #expect(Set(texts) == ["one", "two"])  // neither turn was clobbered
    }

    // MARK: - Context Protocol

    @Test func contextEstimateAndStrategies() async throws {
        func turn(_ role: String, _ content: String) -> JSON {
            .object(["role": .string(role), "content": .string(content)])
        }
        let body = (0..<10).map {
            turn(
                $0 % 2 == 0 ? "user" : "assistant", "turn\($0)-\(String(repeating: "x", count: 80))"
            )
        }
        #expect(AIContext.estimateOne(turn("user", String(repeating: "x", count: 400))) >= 100)
        #expect(AIContext.estimateTokens(body) > AIContext.estimateTokens(Array(body.prefix(2))))

        // truncate: keeps first + recent, omittedMarker, NO summary.
        let tr = AIStrategies.truncate(body: body, budget: 1_000_000)
        #expect(tr.omittedMarker && tr.summary == nil)
        #expect(tr.body.first?["content"].asString == body.first?["content"].asString)
        #expect(tr.body.last?["content"].asString == body.last?["content"].asString)

        // compact: recent verbatim + a summary artifact (via the mock provider).
        let cp = await AIStrategies.compact(
            body: body, recentN: 4, budget: 1_000_000, provider: MockProvider(tokens: ["SUMMARY"]))
        #expect(cp.summary == "SUMMARY")
        #expect(cp.body.count == 4)
    }

    @Test func contextNoticeShapes() async throws {
        let backend = buildAIBackend(baseConfig { .provider(MockProvider()) })
        let n = backend.contextNotice(
            strategy: "compact", summary: "MY SUMMARY", omittedMarker: false, droppedN: 7)
        #expect(n["role"].asString == "user")
        #expect(n["tool_calls"].asArray == nil)
        let c = n["content"].asString ?? ""
        #expect(c.contains("[context-notice]") && c.contains("MY SUMMARY"))
        #expect(c.contains("recall") && c.contains("memory agent") && c.contains("7 earlier turn"))
        let t = backend.contextNotice(
            strategy: "truncate", summary: nil, omittedMarker: true, droppedN: 3)
        let tc = t["content"].asString ?? ""
        #expect(tc.contains("omitted in place") && !tc.contains("Summary of the dropped span"))
    }

    @Test func recallPagesStoreAndContextStatusShape() async throws {
        let (kernel, id) = try await freshMockKernel(
            config: baseConfig { .provider(MockProvider(tokens: ["noted"])) })
        _ = await kernel.send(
            id,
            .object([
                "type": .string("send"),
                "text": .string("my project codename is HALCYON"),
                "client_id": .string("chat"),
            ]))
        try await waitForHistory(kernel: kernel, id: id, client: "chat", count: 2)

        // recall finds the planted fact by substring, full durable history.
        let r = await kernel.send(
            id,
            .object([
                "type": .string("recall"), "query": .string("codename"),
                "client_id": .string("chat"),
            ]))
        let msgs = r["messages"].asArray ?? []
        #expect(
            msgs.contains { ($0["content"].asString ?? "").lowercased().contains("halcyon") })
        #expect(r["client_id"].asString == "chat")

        // context_status shape — default strategy, projection recorded.
        let st = await kernel.send(id, .object(["type": .string("context_status")]))
        #expect(st["context_window"].asInt != nil)
        #expect(st["budget"].asInt != nil)
        #expect(st["strategy"].asString == "compact")
        #expect(st["last_projection"]["fired"].asBool == false)  // under-budget send
        #expect(st["last_reaction"] == .null)  // no compaction fired
    }
}

/// Poll the `history` verb until it reaches `count` messages (the
/// stream runs in a spawned Task, so persistence is async). Bounded so
/// a hung stream fails the test instead of hanging the run.
private func waitForHistory(
    kernel: Kernel, id: AgentId, client: String, count: Int
) async throws {
    for _ in 0..<200 {
        let h = await kernel.send(
            id, .object(["type": .string("history"), "client_id": .string(client)]))
        if (h["messages"].asArray ?? []).count >= count { return }
        try await Task.sleep(nanoseconds: 5_000_000)
    }
}
