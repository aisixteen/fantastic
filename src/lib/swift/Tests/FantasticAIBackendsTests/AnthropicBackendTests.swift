// anthropic_backend — python `tests/test_anthropic_backend.py`, case for case.

import FantasticAICore
import FantasticJSON
import FantasticKernel
import Foundation
import OrderedCollections
import Testing

@testable import FantasticAnthropicBackend

private let MODEL = "claude-test-5"

private func sse(_ events: [JSON]) -> StubHTTP.Reply {
    .sse(events.map { "event: \($0["type"].asString ?? "")\ndata: \($0.serialize())\n\n" }.joined())
}

private func text(_ t: String) -> JSON {
    .object(["type": "content_block_delta", "index": 0, "delta": .object(["type": "text_delta", "text": .string(t)])])
}

private func think(_ t: String) -> JSON {
    .object(["type": "content_block_delta", "index": 0, "delta": .object(["type": "thinking_delta", "thinking": .string(t)])])
}

private let START: JSON = .object([
    "type": "message_start",
    "message": .object(["usage": .object(["input_tokens": 100, "cache_read_input_tokens": 20])]),
])

private func stop(_ reason: String) -> [JSON] {
    [
        .object(["type": "message_delta", "delta": .object(["stop_reason": .string(reason)])]),
        .object(["type": "message_stop"]),
    ]
}

private func provider(_ endpoint: String, key: @escaping KeyReader = { _ in nil }) -> AnthropicProvider {
    AnthropicProvider(
        endpoint: endpoint, model: MODEL, key: key, maxTokens: 8000, temperature: nil, thinkingBudget: nil)
}

private func drain(_ p: AnthropicProvider, maxTokens: Int? = nil) async throws -> String {
    var out = ""
    for try await chunk in p.chat(messages: [["role": "user", "content": "hi"]], maxTokens: maxTokens) {
        if case .token(let t) = chunk { out += t }
    }
    return out
}

private func header(_ r: String, _ name: String) -> String? {
    r.split(separator: "\r\n").first { $0.lowercased().hasPrefix(name + ":") }
        .map { String($0.dropFirst(name.count + 1)).trimmingCharacters(in: .whitespaces) }
}

@Suite("anthropic: the Messages wire, the API read, the optional key")
struct AnthropicBackendTests {

    @Test func systemIsOneStringAndTurnsOfOneRoleMerge() {
        let (system, turns) = toMessages([
            ["role": "system", "content": "a"], ["role": "user", "content": "u1"],
            ["role": "system", "content": "b"], ["role": "user", "content": "u2"],
            ["role": "assistant", "content": "x"],
        ])
        #expect(system == "a\n\nb")
        #expect(turns == [["role": "user", "content": "u1\n\nu2"], ["role": "assistant", "content": "x"]])
    }

    @Test func theBodySendsNoToolsAndOnlyWhatTheRecordSet() {
        let b = provider("http://x/v1/messages").body(
            [["role": "system", "content": "s"], ["role": "user", "content": "hi"]], maxTokens: nil)
        #expect(
            b == .object([
                "model": .string(MODEL), "max_tokens": 8000,
                "messages": [["role": "user", "content": "hi"]], "stream": true, "system": "s",
            ]))
        let p = AnthropicProvider(
            endpoint: "http://x/v1/messages", model: MODEL, key: { _ in nil }, maxTokens: 8000,
            temperature: 0.2, thinkingBudget: 2048)
        let b2 = p.body([["role": "user", "content": "hi"]], maxTokens: nil)
        #expect(b2["temperature"] == .double(0.2))
        #expect(b2["thinking"] == .object(["type": "enabled", "budget_tokens": 2048]))
        let b3 = p.body([["role": "user", "content": "hi"]], maxTokens: 1500)
        #expect(b3["max_tokens"] == 1500 && b3["thinking"] == .null)
    }

    @Test func theKeyRidesAsXApiKeyOnlyWhenNamed() async throws {
        let ok = sse([START, text("ok")] + stop("end_turn"))
        let stub = StubHTTP([ok, ok])
        let ep = "http://127.0.0.1:\(stub.port)/v1/messages"
        _ = try await drain(provider(ep, key: { _ in "sk-test" }))
        _ = try await drain(provider(ep))
        #expect(header(stub.requests[0], "x-api-key") == "sk-test")
        #expect(header(stub.requests[0], "anthropic-version") == "2023-06-01")
        #expect(header(stub.requests[1], "x-api-key") == nil)
    }

    @Test func theThoughtIsWrappedOnceAndTheUsageReported() async throws {
        let stub = StubHTTP([sse([START, think("let me "), think("see"), text("done")] + stop("end_turn"))])
        let p = provider("http://127.0.0.1:\(stub.port)/v1/messages")
        #expect(try await drain(p) == "\(THINK_OPEN)let me see\(THINK_CLOSE)done")
        #expect(p.usage?.lastPromptTokens == 120 && p.usage?.lastStopReason == "end_turn")
    }

    @Test func aStreamWithoutMessageStopDidNotFinish() async {
        let stub = StubHTTP([sse([START, text("half")])])
        await #expect(throws: PyError.self) { _ = try await drain(provider("http://127.0.0.1:\(stub.port)/v1/messages")) }
    }

    @Test func anErrorEventCarriesTheApisOwnMessage() async {
        let err: JSON = .object(["type": "error", "error": .object(["type": "overloaded_error", "message": "Overloaded"])])
        let stub = StubHTTP([sse([START, err])])
        do {
            _ = try await drain(provider("http://127.0.0.1:\(stub.port)/v1/messages"))
            Issue.record("no error")
        } catch {
            #expect(pyErrorText(error).contains("stream error: overloaded_error: Overloaded"), "\(error)")
        }
    }

    @Test func aRefusedKeySaysToNameCredentials() async {
        let stub = StubHTTP([.init(status: 401, body: "{\"type\":\"error\"}")])
        do {
            _ = try await drain(provider("http://127.0.0.1:\(stub.port)/v1/messages"))
            Issue.record("no error")
        } catch {
            #expect(pyErrorText(error).contains("name it in `credentials`"), "\(error)")
        }
    }

    @Test func aCutMainTurnFailsAndACutSideCallIsItsAnswer() async throws {
        let cut = sse([START, text("half")] + stop("max_tokens"))
        let stub = StubHTTP([cut, cut])
        let ep = "http://127.0.0.1:\(stub.port)/v1/messages"
        do {
            _ = try await drain(provider(ep))
            Issue.record("no cut")
        } catch {
            #expect(pyErrorText(error).contains("cut at max_tokens=8000"), "\(error)")
        }
        let p = provider(ep)
        #expect(try await drain(p, maxTokens: 500) == "half")
        #expect(p.usage?.lastStopReason == "max_tokens")
    }

    // ─── the write ──────────────────────────────────────────────

    private func rec(_ extra: OrderedDictionary<String, JSON> = [:]) -> OrderedDictionary<String, JSON> {
        var r: OrderedDictionary<String, JSON> = [
            "endpoint": "https://api.test/v1/messages", "model": .string(MODEL), "memory_store_id": "memory",
        ]
        for (k, v) in extra { r[k] = v }
        return r
    }

    @Test func endpointModelAndAStoreAreAllARecordNeeds() {
        let required = (AnthropicBundle.RECORD_SCHEMA["fields"].asArray ?? [])
            .filter { $0["required"].asBool == true }.compactMap { $0["name"].asString }
        #expect(required == ["endpoint", "model", "memory_store_id"])
        #expect(AnthropicBundle().validateMeta(rec(), record: [:]) == nil)
    }

    @Test func aWriteTheBackendCannotHonourIsRefused() {
        let cases: [(OrderedDictionary<String, JSON>, String)] = [
            (["max_tokens": 0], "max_tokens must be a positive int"),
            (["thinking_budget": 512], "below the API's 1024"),
            (["thinking_budget": 4000, "max_tokens": 4000], "not below max_tokens=4000"),
            (["temperature": "hot"], "temperature must be a number"),
            (["endpoint": "https://api.test/v1/chat"], "max_tokens is left to the API, but the endpoint"),
        ]
        for (meta, fragment) in cases {
            let err = AnthropicBundle().validateMeta(meta, record: rec()) ?? ""
            #expect(err.contains(fragment), "\(meta): \(err)")
        }
    }

    // ─── the read ───────────────────────────────────────────────

    private let entry = #"{"id": "claude-test-5", "max_input_tokens": 200000, "max_tokens": 64000}"#

    @Test func whatTheRecordLeavesOutIsReadFromTheModelEntry() async throws {
        let stub = StubHTTP([.init(body: entry), .init(body: entry)])
        let ep = "http://127.0.0.1:\(stub.port)/v1/messages"
        let got = try await AnthropicServerRead.read(rec(["endpoint": .string(ep)]), key: { _ in "sk-test" })
        #expect(got == ["max_tokens": 64000, "context_window": 200000])
        #expect(stub.requests[0].hasPrefix("GET /v1/models/\(MODEL) "))
        #expect(header(stub.requests[0], "x-api-key") == "sk-test")
        let got2 = try await AnthropicServerRead.read(
            rec(["endpoint": .string(ep), "max_tokens": 4000]), key: { _ in nil })
        #expect(got2 == ["context_window": 200000])
    }

    @Test func anApiThatDoesNotAnswerRefusesNamingTheField() async {
        let stub = StubHTTP([.init(status: 404, body: "{}")])
        let ep = "http://127.0.0.1:\(stub.port)/v1/messages"
        await #expect(
            throws: AnthropicServerRead.Failure(
                field: "max_tokens",
                message: "max_tokens: GET http://127.0.0.1:\(stub.port)/v1/models/\(MODEL) answered "
                    + "HTTP 404; set max_tokens on the record")
        ) { try await AnthropicServerRead.read(rec(["endpoint": .string(ep)]), key: { _ in nil }) }
        let stub2 = StubHTTP([.init(body: #"{"id": "claude-test-5"}"#)])
        let ep2 = "http://127.0.0.1:\(stub2.port)/v1/messages"
        do {
            _ = try await AnthropicServerRead.read(rec(["endpoint": .string(ep2), "max_tokens": 10]), key: { _ in nil })
            Issue.record("no refusal")
        } catch {
            #expect(error.field == "context_window" && error.message.contains("names no max_input_tokens"))
        }
    }

    // ─── through the kernel ─────────────────────────────────────

    @Test func aSendReadsTheApiFirstAndANoTurnVerbReadsNothing() async {
        let stub = StubHTTP([.init(body: entry)])
        let registry = BundleRegistry()
        registry.register(ANTHROPIC_HANDLER_MODULE, AnthropicBundle())
        let k = Kernel(storage: .inMemory, bundles: registry)
        let root = Agent(id: AgentId("kernel_state"), handlerModule: nil, parentId: nil)
        k.register(root)
        k.setRoot(root)
        var payload: OrderedDictionary<String, JSON> = [
            "type": "create_agent", "parent_id": "kernel_state", "id": "a1",
            "handler_module": .string(ANTHROPIC_HANDLER_MODULE),
        ]
        for (key, v) in rec(["endpoint": .string("http://127.0.0.1:\(stub.port)/v1/messages")]) { payload[key] = v }
        let r = await k.send([Item(AgentId("kernel_state"), .object(payload))])[0].result
        #expect(r["id"] == "a1", "\(r)")
        for verb in ["reflect", "status", "context_status", "rows"] {
            _ = await k.send([Item(AgentId("a1"), .object(["type": .string(verb)]))])[0]
        }
        #expect(stub.requests.isEmpty)
        _ = await k.send([Item(AgentId("a1"), ["type": "send", "text": "hi", "client_id": "t"])])[0]
        let meta = k.agent(AgentId("a1"))?.meta ?? [:]
        #expect(meta["max_tokens"] == 64000 && meta["context_window"] == 200000, "\(meta)")
    }

    @Test func readmeIsPythonVerbatim() throws {
        let py = URL(fileURLWithPath: #filePath)
            .deletingLastPathComponent().deletingLastPathComponent()
            .deletingLastPathComponent().deletingLastPathComponent()
            .appendingPathComponent(
                "python/bundled_agents/ai/anthropic/anthropic_backend/src/anthropic_backend/readme.md")
        #expect(AnthropicBundle().readme == (try String(contentsOf: py, encoding: .utf8)))
    }
}
