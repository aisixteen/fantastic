// Each provider's chat STREAM BODY, pinned end to end over the loopback stub.
//
// `ProviderPolicyTests` pins what a missing field says and which URL the
// record's endpoint builds, and the per-backend decode tests pin the pure line
// decoders. What sat unexercised was the streaming task each provider wraps
// around them: the Responses provider's whole HTTP leg, ollama's non-2xx
// branch, the usage each stream reports, the request bytes each one sends, and
// how a stream that ends early is closed. These are the behaviours a shared
// streaming-POST helper would have to reproduce exactly.

import FantasticAICore
import FantasticJSON
import FantasticKernel
import Foundation
import OrderedCollections
import Testing

@testable import FantasticOllamaBackend
@testable import FantasticResponsesBackend
@testable import FantasticVllmBackend

private func drain(_ stream: AsyncThrowingStream<AIChunk, Error>) async -> (
    text: String, error: Error?
) {
    var text = ""
    do {
        for try await chunk in stream {
            if case .token(let t) = chunk { text += t }
        }
        return (text, nil)
    } catch {
        return (text, error)
    }
}

/// The JSON body of the n-th request the stub received, as sent.
private func bodyText(_ stub: StubHTTP, _ n: Int = 0) -> String {
    let raw = stub.requests[n]
    guard let r = raw.range(of: "\r\n\r\n") else { return "" }
    return String(raw[r.upperBound...])
}

/// The request line + headers of the n-th request, lowercased.
private func head(_ stub: StubHTTP, _ n: Int = 0) -> String {
    let raw = stub.requests[n]
    guard let r = raw.range(of: "\r\n\r\n") else { return raw.lowercased() }
    return String(raw[..<r.lowerBound]).lowercased()
}

private func sse(_ events: [String]) -> StubHTTP.Reply {
    .sse(events.map { "data: \($0)" }.joined(separator: "\n\n") + "\n\n")
}

private let oneUser: [JSON] = [.object(["role": "user", "content": "hi"])]
private let withSystem: [JSON] = [
    .object(["role": "system", "content": "be terse"]),
    .object(["role": "user", "content": "hi"]),
]

// ── responses_backend ───────────────────────────────────────────────────

@Suite("Responses provider — the HTTP leg")
struct ResponsesStreamPinTests {

    private func provider(
        _ port: Int, key: String? = "sk-test", endpoint: String? = nil, model: String = "m"
    ) -> ResponsesProvider {
        try! ResponsesProvider(
            endpoint: endpoint ?? "http://127.0.0.1:\(port)/v1/responses", model: model,
            key: key.map(fixedKey) ?? refusedKey("responses_backend: credentials creds/openai: no such ref"),
            maxTokens: 900, sampling: ["temperature": .double(0.2)])
    }

    @Test func aTurnStreamsBothChannelsReportsUsageAndSendsTheBodyVerbatim() async {
        let stub = StubHTTP([
            sse([
                #"{"type":"response.reasoning_text.delta","delta":"hm"}"#,
                #"{"type":"response.output_text.delta","delta":"out"}"#,
                #"{"type":"response.completed","response":{"status":"completed","usage":{"input_tokens":42}}}"#,
            ])
        ])
        defer { stub.stop() }
        let p = provider(stub.port)
        let (text, error) = await drain(p.chat(messages: withSystem))
        #expect(error == nil, "\(String(describing: error))")
        #expect(text == "<think>hm</think>out")
        #expect(p.usage?.lastPromptTokens == 42)
        #expect(p.usage?.lastStopReason == "completed")
        #expect(stub.requests.count == 1)
        let h = head(stub)
        #expect(h.hasPrefix("post /v1/responses http/1.1"), "\(h)")
        #expect(h.contains("authorization: bearer sk-test"))
        #expect(h.contains("accept: text/event-stream"))
        #expect(h.contains("content-type: application/json"))
        #expect(bodyText(stub) == JSON.object(p.requestBody(messages: withSystem)).serialize())
        #expect(
            bodyText(stub)
                == #"{"model":"m","input":[{"type":"message","role":"user","content":[{"type":"input_text","text":"hi"}]}],"#
                + #""max_output_tokens":900,"stream":true,"store":false,"instructions":"be terse","temperature":0.2}"#
        )
    }

    @Test func aStatusErrorRaisesWithTheCodeAndTheBody() async {
        let stub = StubHTTP([
            .init(
                status: 400, headers: [("Content-Type", "application/json")],
                body: #"{"error":"bad model"}"#)
        ])
        defer { stub.stop() }
        let (_, error) = await drain(provider(stub.port).chat(messages: oneUser))
        #expect("\(error!)" == #"RuntimeError: responses 400: {"error":"bad model"}"#)
        #expect(stub.requests.count == 1)
    }

    /// A transport code with a NON-JSON body is retried (twice at most), and
    /// each retry prints python's line; the same code with a JSON body is the
    /// API speaking and is NOT retried.
    @Test func aGatewayPageIsRetriedAndLoggedAJsonBodyIsNot() async {
        let page = StubHTTP.Reply(status: 503, body: "<html>gateway</html>")
        let good = sse([
            #"{"type":"response.output_text.delta","delta":"ok"}"#,
            #"{"type":"response.completed","response":{"status":"completed"}}"#,
        ])
        let stub = StubHTTP([page, good])
        defer { stub.stop() }
        var result: (text: String, error: Error?) = ("", nil)
        let lines = await RuntimeLog.captureOut {
            result = await drain(provider(stub.port).chat(messages: oneUser))
        }
        #expect(result.error == nil && result.text == "ok")
        #expect(stub.requests.count == 2)
        #expect(
            lines.contains("[responses] 503 (no content-type) — transport error, retry 1/2"),
            "\(lines)")

        let json = StubHTTP([
            .init(
                status: 503, headers: [("Content-Type", "application/json")],
                body: #"{"error":"overloaded"}"#),
            good,
        ])
        defer { json.stop() }
        let (_, error) = await drain(provider(json.port).chat(messages: oneUser))
        #expect(pyErrorText(error!) == #"responses 503: {"error":"overloaded"}"#)
        #expect(json.requests.count == 1)
    }

    @Test func transportRetriesGiveUpAfterTwo() async {
        let page = StubHTTP.Reply(
            status: 502, headers: [("Content-Type", "text/html")], body: "bad gateway")
        let stub = StubHTTP([page, page, page])
        defer { stub.stop() }
        var error: Error?
        let lines = await RuntimeLog.captureOut {
            error = await drain(provider(stub.port).chat(messages: oneUser)).error
        }
        #expect(pyErrorText(error!) == "responses 502: bad gateway")
        #expect(stub.requests.count == 3)
        #expect(
            lines.filter { $0.hasPrefix("[responses] 502 (text/html)") }
                == [
                    "[responses] 502 (text/html) — transport error, retry 1/2",
                    "[responses] 502 (text/html) — transport error, retry 2/2",
                ], "\(lines)")
    }

    /// The endpoint is checked first, then the model, then the key is read (a
    /// refused read raises the store's text) — and none of them reaches the wire.
    /// Python `test_the_missing_config_texts_are_exact`.
    @Test func endpointThenModelThenKeyFailInsideTheTurn() async {
        let stub = StubHTTP([])
        defer { stub.stop() }
        let noKey = await drain(provider(stub.port, key: nil).chat(messages: oneUser))
        #expect(
            pyErrorText(noKey.error!) == "responses_backend: credentials creds/openai: no such ref")
        let noEndpoint = await drain(
            provider(stub.port, key: nil, endpoint: "", model: "").chat(messages: oneUser))
        #expect(
            "\(noEndpoint.error!)"
                == "RuntimeError: responses_backend: no `endpoint` set — this connector does not "
                + "guess a host. Name the full …/v1/responses URL on the agent record (endpoint=...)."
        )
        let noModel = await drain(provider(stub.port, key: nil, model: "").chat(messages: oneUser))
        #expect(
            "\(noModel.error!)"
                == "RuntimeError: responses_backend: no `model` set — this connector does not guess "
                + "one. Name a model on the agent record (model=...).")
        #expect(stub.requests.isEmpty)
    }

    @Test func aStreamCutMidThoughtClosesTheSpanThenFails() async {
        let stub = StubHTTP([sse([#"{"type":"response.reasoning_text.delta","delta":"half"}"#])])
        defer { stub.stop() }
        let (text, error) = await drain(provider(stub.port).chat(messages: oneUser))
        #expect(text == "<think>half</think>")
        #expect(
            pyErrorText(error!)
                == "responses stream ended without completing: status=None reason=None items=[] "
                + "(no terminal event — connection cut mid-stream)")
    }

    /// A cut at `max_output_tokens` is an ANSWER on a bounded side call and an
    /// error on a main turn; both report `max_tokens`.
    @Test func aCutAtTheBoundIsAnAnswerOnlyOnASideCall() async {
        let cut = sse([
            #"{"type":"response.output_text.delta","delta":"half a "}"#,
            #"{"type":"response.incomplete","response":{"status":"incomplete","incomplete_details":{"reason":"max_output_tokens"}}}"#,
        ])
        let stub = StubHTTP([cut, cut])
        defer { stub.stop() }
        let side = provider(stub.port)
        let (sideText, sideError) = await drain(side.chat(messages: oneUser, maxTokens: 64))
        #expect(sideError == nil && sideText == "half a ")
        #expect(side.usage?.lastStopReason == "max_tokens")
        #expect(bodyText(stub, 0).contains(#""max_output_tokens":64"#))
        let main = provider(stub.port)
        let (_, mainError) = await drain(main.chat(messages: oneUser))
        #expect(
            pyErrorText(mainError!)
                == "responses stream ended without completing: status='incomplete' "
                + "reason='max_output_tokens' items=[]")
        #expect(main.usage?.lastStopReason == "max_tokens")
    }

    /// A failed/error event ends the turn AT ONCE — the open think span is not
    /// closed on this path (python raises from inside the loop).
    @Test func aFailedEventRaisesWithoutClosingTheSpan() async {
        let stub = StubHTTP([
            sse([
                #"{"type":"response.reasoning_text.delta","delta":"x"}"#,
                #"{"type":"response.failed","response":{"error":{"message":"upstream died"}}}"#,
                #"{"type":"response.output_text.delta","delta":"never"}"#,
            ])
        ])
        defer { stub.stop() }
        let (text, error) = await drain(provider(stub.port).chat(messages: oneUser))
        #expect(text == "<think>x")
        #expect(
            "\(error!)" == "RuntimeError: responses stream failed: {'message': 'upstream died'}")
    }
}

// ── ollama_backend ──────────────────────────────────────────────────────

@Suite("Ollama provider — the HTTP leg")
struct OllamaStreamPinTests {

    private func provider(
        _ port: Int, model: String = "m", numCtx: Int64? = nil, numPredict: Int64? = nil,
        think: Bool? = nil
    ) -> OllamaProvider {
        OllamaProvider(
            endpoint: "http://127.0.0.1:\(port)", model: model, temperature: 0.3, numCtx: numCtx,
            numPredict: numPredict, think: think)
    }

    /// python's ollama client: `ResponseError(<error>, <code>)`, whose text is
    /// `<error> (status code: <n>)` — the body's JSON `error` when it parses,
    /// else the raw body.
    @Test func aNon2xxRaisesResponseErrorWithTheBodysError() async {
        let stub = StubHTTP([
            .init(
                status: 500, headers: [("Content-Type", "application/json")],
                body: #"{"error":"model not found"}"#),
            .init(status: 503, body: "busy"),
            .init(status: 404, body: #"{"detail":"x"}"#),
        ])
        defer { stub.stop() }
        let a = await drain(provider(stub.port).chat(messages: oneUser))
        #expect("\(a.error!)" == "ResponseError: model not found (status code: 500)")
        #expect(pyErrorText(a.error!) == "model not found (status code: 500)")
        let b = await drain(provider(stub.port).chat(messages: oneUser))
        #expect("\(b.error!)" == "ResponseError: busy (status code: 503)")
        let c = await drain(provider(stub.port).chat(messages: oneUser))
        #expect("\(c.error!)" == #"ResponseError: {"detail":"x"} (status code: 404)"#)
        #expect(stub.requests.count == 3, "no retry")
    }

    @Test func aTurnReportsUsageAndTheLengthStop() async {
        let stub = StubHTTP([
            .init(
                body: [
                    #"{"message":{"thinking":"hm"},"done":false}"#,
                    #"{"message":{"content":"ok"},"done":false}"#,
                    #"{"message":{"content":""},"done":true,"done_reason":"length","prompt_eval_count":77}"#,
                ].joined(separator: "\n") + "\n")
        ])
        defer { stub.stop() }
        let p = provider(stub.port)
        let (text, error) = await drain(p.chat(messages: oneUser))
        #expect(error == nil)
        #expect(text == "<think>hm</think>ok")
        #expect(p.usage?.lastPromptTokens == 77)
        #expect(p.usage?.lastStopReason == "max_tokens")
        let h = head(stub)
        #expect(h.hasPrefix("post /api/chat http/1.1"))
        #expect(h.contains("content-type: application/json"))
    }

    /// A stream that ends with no `done` line still closes its think span and
    /// is NOT an error (ollama's provider checks no terminality).
    @Test func aCutStreamClosesTheSpanAndEndsCleanly() async {
        let stub = StubHTTP([.init(body: #"{"message":{"thinking":"half"},"done":false}"#)])
        defer { stub.stop() }
        let (text, error) = await drain(provider(stub.port).chat(messages: oneUser))
        #expect(error == nil)
        #expect(text == "<think>half</think>")
    }

    /// python's body: model, messages, stream, options{temperature, num_ctx?,
    /// num_predict?}, then `think` only when set. A side call's bound replaces
    /// the record's num_predict.
    @Test func theRequestBodyKeyOrder() async {
        let done = StubHTTP.Reply(body: #"{"message":{"content":"x"},"done":true}"#)
        let stub = StubHTTP([done, done, done])
        defer { stub.stop() }
        _ = await drain(provider(stub.port).chat(messages: oneUser))
        _ = await drain(
            provider(stub.port, numCtx: 8192, numPredict: 500, think: false).chat(messages: oneUser)
        )
        _ = await drain(
            provider(stub.port, numPredict: 500, think: true).chat(messages: oneUser, maxTokens: 64)
        )
        let msgs = #""messages":[{"role":"user","content":"hi"}]"#
        #expect(
            bodyText(stub, 0) == #"{"model":"m","# + msgs
                + #","stream":true,"options":{"temperature":0.3}}"#)
        #expect(
            bodyText(stub, 1)
                == #"{"model":"m","# + msgs
                + #","stream":true,"options":{"temperature":0.3,"num_ctx":8192,"num_predict":500},"think":false}"#
        )
        #expect(
            bodyText(stub, 2)
                == #"{"model":"m","# + msgs
                + #","stream":true,"options":{"temperature":0.3,"num_predict":64},"think":true}"#)
    }

    @Test func anEmptyModelRaisesInsideTheTurn() async {
        let stub = StubHTTP([])
        defer { stub.stop() }
        let (_, error) = await drain(provider(stub.port, model: "").chat(messages: oneUser))
        #expect(
            "\(error!)"
                == "RuntimeError: ollama_backend: no `model` set — this connector does not guess one. "
                + "Name a model on the agent record (model=...).")
        #expect(stub.requests.isEmpty)
    }
}

// ── vllm_backend ────────────────────────────────────────────────────────

@Suite("vLLM provider — a malformed data line")
struct VllmStreamPinTests {

    /// python's `json.loads` raises on a data line that is not JSON, and so does
    /// this: the turn fails as a `JSONDecodeError`, not a transport sentence.
    @Test func aDataLineThatIsNotJsonFailsTheTurn() async {
        let stub = StubHTTP([
            .sse(
                "data: {\"choices\":[{\"delta\":{\"content\":\"a\"},\"finish_reason\":null}]}\n\n"
                    + "data: {not json\n\n")
        ])
        defer { stub.stop() }
        let p = VllmProvider(
            endpoint: "http://127.0.0.1:\(stub.port)/\(UUID().uuidString.prefix(8))/v1",
            model: "q", key: fixedKey(nil), maxTokens: 8000, kvSlots: 7, kvOverflow: "evict", kvTtl: KV_TTL,
            sampling: SAMPLING_DEFAULTS, explicitBudget: false,
            connectTimeout: CONNECT_TIMEOUT, readTimeout: READ_TIMEOUT,
            agentKey: UUID().uuidString,
            kernelSerial: Kernel(storage: .inMemory, bundles: BundleRegistry()).serial,
            clock: { ProcessInfo.processInfo.systemUptime })
        let (text, error) = await drain(p.chat(messages: oneUser))
        #expect(text == "a")
        #expect((error as? PyError)?.type == "JSONDecodeError", "\(String(describing: error))")
        #expect("\(error!)".hasPrefix("JSONDecodeError: "))
    }
}
