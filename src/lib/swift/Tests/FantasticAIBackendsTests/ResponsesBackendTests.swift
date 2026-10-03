// The Responses wire, pinned — python `responses_backend/tests/test_responses_provider.py`
// is the oracle. The decode and the request body are pure functions; the HTTP
// plumbing is exercised live by the e2e `backends` group.

import FantasticAICore
import FantasticJSON
import Foundation
import Testing

@testable import FantasticResponsesBackend

private func line(_ obj: JSON) -> String { "data: " + obj.serialize() }

private func decode(_ events: [JSON]) -> (text: String, state: ResponsesStreamState, error: String?, prompt: Int?) {
    var st = ResponsesStreamState()
    var out = ""
    var err: String? = nil
    var prompt: Int? = nil
    for e in events {
        let d = decodeResponsesSSELine(line(e), state: &st)
        out += d.tokens.joined()
        if let n = d.promptTokens { prompt = n }
        if err == nil { err = d.error }
        if d.done || err != nil { break }
    }
    out += st.wrap.finish().joined()
    return (out, st, err, prompt)
}

private func done(_ status: String, reason: String? = nil, input: Int64 = 42) -> JSON {
    let usage: JSON = ["input_tokens": .integer(input)]
    let resp: JSON =
        reason.map {
            ["status": .string(status), "usage": usage, "incomplete_details": ["reason": .string($0)]]
        } ?? ["status": .string(status), "usage": usage]
    return [
        "type": .string(status == "completed" ? "response.completed" : "response.incomplete"),
        "response": resp,
    ]
}

private func delta(_ type: String, _ text: String) -> JSON {
    .object(["type": .string("response.\(type).delta"), "delta": .string(text)])
}

private let msgs: [JSON] = [
    .object(["role": "system", "content": "be terse"]),
    .object(["role": "user", "content": "hi"]),
    .object(["role": "assistant", "content": "hello"]),
    .object(["role": "user", "content": "again"]),
]

private func provider(
    endpoint: String = "http://proxy.test/v1/responses", model: String = "qwen-test",
    key: @escaping KeyReader = fixedKey("sk"), maxTokens: Int64 = 900
) -> ResponsesProvider {
    try! ResponsesProvider(
        endpoint: endpoint, model: model, key: key, maxTokens: maxTokens, sampling: [:])
}

private func firstError(_ p: ResponsesProvider) async -> String {
    do {
        for try await _ in p.chat(messages: msgs) {}
        return ""
    } catch {
        return "\(error)"
    }
}

@Suite("Responses backend")
struct ResponsesBackendTests {

    /// Audit S1: the client's refusal of a header value QUOTES the key — it
    /// leaves as a fixed text, rust's; an error body or an error event echoing the
    /// key leaves with the key as `<secret>`.
    @Test func theKeyNeverLeavesInAnErrorText() async {
        let echo = StubHTTP.Reply(
            status: 401, headers: [("Content-Type", "application/json")],
            body: "invalid key: Bearer sk-test-fixture")
        let event = StubHTTP.Reply.sse(
            "data: {\"type\":\"error\",\"message\":\"bad key sk-test-fixture\"}\n\n")
        let stub = StubHTTP([echo, event])
        defer { stub.stop() }
        let ep = "http://127.0.0.1:\(stub.port)/v1/responses"
        // refused BEFORE any connection: nothing listens on port 1
        let refused = await firstError(
            provider(
                endpoint: "http://127.0.0.1:1/v1/responses", key: fixedKey("sk-test-fixture\n")))
        #expect(
            refused == "RuntimeError: api_key has illegal header chars: failed to parse header value",
            "\(refused)")
        let echoed = await firstError(provider(endpoint: ep, key: fixedKey("sk-test-fixture")))
        #expect(echoed == "RuntimeError: responses 401: invalid key: Bearer <secret>", "\(echoed)")
        let streamed = await firstError(provider(endpoint: ep, key: fixedKey("sk-test-fixture")))
        #expect(
            streamed
                == "RuntimeError: responses stream error: {'type': 'error', 'message': 'bad key <secret>'}",
            "\(streamed)")
    }

    @Test func readmeIsPythonVerbatim() throws {
        let py = URL(fileURLWithPath: #filePath)
            .deletingLastPathComponent().deletingLastPathComponent()
            .deletingLastPathComponent().deletingLastPathComponent()
            .appendingPathComponent(
                "python/bundled_agents/ai/responses/responses_backend/src/responses_backend/readme.md")
        #expect(ResponsesBundle().readme == (try String(contentsOf: py, encoding: .utf8)))
    }

    @Test func messagesTranslateToInstructionsAndTypedItems() {
        let (ins, items) = translateResponsesMessages([
            .object(["role": "system", "content": "first"]),
            .object(["role": "system", "content": "last wins"]),
            .object(["role": "user", "content": "hi"]),
            .object(["role": "assistant", "content": ""]),
            .object(["role": "tool", "content": "dropped"]),
        ])
        #expect(ins == "last wins")
        #expect(items.count == 2)
        #expect(items[0]["content"][0]["type"].asString == "input_text")
        #expect(items[1]["content"][0]["type"].asString == "output_text")
        #expect(items[1]["content"][0]["text"].asString == "(no content)")
    }

    @Test func bodyIsStatelessNoToolsInPythonsKeyOrder() {
        let p = try! ResponsesProvider(
            endpoint: "http://proxy.test/v1/responses", model: "qwen-test", key: fixedKey("sk"),
            maxTokens: 900, sampling: ["temperature": .double(0.2)])
        let body = p.requestBody(messages: msgs)
        #expect(Array(body.keys) == [
            "model", "input", "max_output_tokens", "stream", "store", "instructions", "temperature",
        ])
        #expect(body["max_output_tokens"] == .integer(900))
        #expect(body["store"] == .bool(false))
        #expect(body["instructions"] == .string("be terse"))
        #expect(body["input"]?.asArray?.count == 3)
        #expect(body["tools"] == nil)
        #expect(p.requestBody(messages: msgs, maxTokens: 64)["max_output_tokens"] == .integer(64))
        #expect(p.requestBody(messages: msgs, maxTokens: 0)["max_output_tokens"] == .integer(900))
    }

    @Test func thinkingIsWrappedNotDropped() {
        let r = decode([
            delta("reasoning_text", "let me "), delta("reasoning_text", "think"),
            delta("output_text", "answer"), done("completed", input: 7),
        ])
        #expect(r.text == "<think>let me think</think>answer")
        #expect(r.prompt == 7)
        #expect(responsesTerminalityError(r.state) == nil)
        #expect(responsesStopReason(r.state) == "completed")
    }

    @Test func aReasoningSummaryIsTheThoughtChannelToo() {
        let r = decode([delta("reasoning_summary_text", "sum"), delta("output_text", "out"), done("completed")])
        #expect(r.text == "<think>sum</think>out")
    }

    @Test func anAllThoughtTurnStillClosesTheWrapper() {
        #expect(decode([delta("reasoning_text", "hm"), done("completed")]).text == "<think>hm</think>")
    }

    @Test func aCutIsVisibleAndReportsMaxTokens() {
        let r = decode([delta("output_text", "half a "), done("incomplete", reason: "max_output_tokens")])
        #expect(responsesStopReason(r.state) == "max_tokens")
        let why = responsesTerminalityError(r.state) ?? ""
        #expect(why.contains("without completing") && why.contains("max_output_tokens"))
    }

    @Test func noTerminalEventIsACutConnection() {
        let r = decode([delta("output_text", "x")])
        #expect((responsesTerminalityError(r.state) ?? "").contains("connection cut mid-stream"))
    }

    @Test func responseFailedAndANativeFunctionCallAreLoud() {
        let failed = decode([
            .object([
                "type": "response.failed",
                "response": .object(["error": .object(["message": "upstream died"])]),
            ])
        ])
        #expect((failed.error ?? "").contains("upstream died"))
        let call = decode([
            .object([
                "type": "response.output_item.added",
                "item": .object(["type": "function_call", "name": "send"]),
            ])
        ])
        #expect((call.error ?? "").contains("native function_call"))
        #expect((call.error ?? "").contains("<call name="))
    }

    @Test func missingKeyEndpointOrModelFailLoudNamingIt() async {
        #expect(
            await firstError(
                provider(key: refusedKey("responses_backend: credentials creds/openai: no such ref"))
            ).contains("credentials"))
        #expect(await firstError(provider(endpoint: "")).contains("no `endpoint` set"))
        #expect(await firstError(provider(model: "")).contains("no `model` set"))
    }

    /// `store:false` keeps the conversation ours and `model` is the record's — a
    /// `sampling` key naming either is refused, not merged over it (audit d8).
    @Test func samplingMayNotOverrideWhatTheProviderOwns() {
        for key in ["store", "model", "max_output_tokens"] {
            #expect(throws: PyError.self) {
                try ResponsesProvider(
                    endpoint: "http://x/v1/responses", model: "m", key: fixedKey("sk"), maxTokens: 64,
                    sampling: [key: .integer(1)])
            }
            do throws(PyError) {
                _ = try ResponsesProvider(
                    endpoint: "http://x/v1/responses", model: "m", key: fixedKey("sk"), maxTokens: 64,
                    sampling: [key: .integer(1)])
            } catch {
                #expect(error.message.contains(key), "\(error)")
            }
        }
    }

    @Test func recordFieldsMapLikePython() {
        #expect(ResponsesBundle.maxTokens(nil) == Int64(DEFAULT_MAX_TOKENS))
        #expect(ResponsesBundle.maxTokens(.integer(0)) == Int64(DEFAULT_MAX_TOKENS))
        #expect(ResponsesBundle.maxTokens(.integer(1234)) == 1234)
        let s = ResponsesBundle.sampling(.object(["top_p": .null, "temperature": .double(0.1)]))
        #expect(Array(s.keys) == ["temperature"])
    }
}
