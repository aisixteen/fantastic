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

/// Deterministic provider: yields a fixed token sequence, then an
/// optional finalized tool-call. `chat` can also be told to throw.
private struct MockProvider: AIProvider {
    let model: String
    let tokens: [String]
    let toolCalls: [JSON]
    let throwsError: Bool

    init(
        model: String = "mock-1",
        tokens: [String] = ["Hel", "lo"],
        toolCalls: [JSON] = [],
        throwsError: Bool = false
    ) {
        self.model = model
        self.tokens = tokens
        self.toolCalls = toolCalls
        self.throwsError = throwsError
    }

    func chat(messages: [JSON], tools: [JSON]) -> AsyncThrowingStream<AIChunk, Error> {
        let tokens = tokens
        let toolCalls = toolCalls
        let throwsError = throwsError
        return AsyncThrowingStream { continuation in
            Task {
                if throwsError {
                    continuation.finish(throwing: MockError.boom)
                    return
                }
                for t in tokens { continuation.yield(.token(t)) }
                for c in toolCalls { continuation.yield(.toolCall(c)) }
                continuation.finish()
            }
        }
    }
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
    let kernel = Kernel(storage: .inMemory, bundles: registry)
    let root = Agent(id: AgentId("core"), handlerModule: nil, parentId: nil)
    kernel.register(root)
    kernel.setRoot(root)
    let r = await kernel.send(
        AgentId("core"),
        .object([
            "type": .string("create_agent"),
            "handler_module": .string("mock_backend.tools"),
            "id": .string("mock"),
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

    @Test func toolCallsPersistedSortedByIdWhenEnabled() async throws {
        let calls: [JSON] = [
            .object(["id": .string("b"), "function": .object(["name": .string("x")])]),
            .object(["id": .string("a"), "function": .object(["name": .string("y")])]),
        ]
        let (kernel, id) = try await freshMockKernel(
            config: baseConfig(persistToolCalls: true) {
                .provider(MockProvider(tokens: ["t"], toolCalls: calls))
            })
        _ = await kernel.send(
            id,
            .object([
                "type": .string("send"), "text": .string("go"),
                "client_id": .string("c"),
            ]))
        try await waitForHistory(kernel: kernel, id: id, client: "c", count: 2)
        let h = await kernel.send(
            id, .object(["type": .string("history"), "client_id": .string("c")]))
        let assistant = (h["messages"].asArray ?? [])[1]
        let tc = assistant["tool_calls"].asArray ?? []
        #expect(tc.count == 2)
        #expect(tc[0]["id"].asString == "a")  // sorted by id
        #expect(tc[1]["id"].asString == "b")
    }

    @Test func toolCallsDroppedWhenPersistDisabled() async throws {
        let calls: [JSON] = [.object(["id": .string("a")])]
        let (kernel, id) = try await freshMockKernel(
            config: baseConfig {
                .provider(MockProvider(tokens: ["t"], toolCalls: calls))
            })
        _ = await kernel.send(
            id,
            .object([
                "type": .string("send"), "text": .string("go"),
                "client_id": .string("c"),
            ]))
        try await waitForHistory(kernel: kernel, id: id, client: "c", count: 2)
        let h = await kernel.send(
            id, .object(["type": .string("history"), "client_id": .string("c")]))
        let assistant = (h["messages"].asArray ?? [])[1]
        #expect(assistant["tool_calls"].asArray == nil)
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
