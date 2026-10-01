// Each provider's chat STREAM BODY, pinned end to end over the loopback stub.
//
// `ProviderPolicyTests` pins the retry/status POLICY for anthropic and nvidia,
// and the per-backend decode tests pin the pure line decoders. What sat
// unexercised was the streaming task each provider wraps around them: the
// Responses provider's whole HTTP leg, ollama's and llama.cpp's non-2xx
// branches, the usage each stream reports, the request bytes each one sends,
// and how a stream that ends early is closed. These are the behaviours a shared
// streaming-POST helper would have to reproduce exactly.

import FantasticAICore
import FantasticJSON
import FantasticKernel
import Foundation
import OrderedCollections
import Testing

@testable import FantasticAnthropicBackend
@testable import FantasticLlamaCppBackend
@testable import FantasticNvidiaNimBackend
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
            apiKey: key, maxTokens: 900, sampling: ["temperature": .double(0.2)])
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

    /// The key is checked first, then the endpoint, then the model — and none of
    /// them reaches the wire.
    @Test func keyThenEndpointThenModelFailInsideTheTurn() async {
        let stub = StubHTTP([])
        defer { stub.stop() }
        let noKey = await drain(
            provider(stub.port, key: nil, endpoint: "", model: "").chat(messages: oneUser))
        #expect("\(noKey.error!)" == "RuntimeError: OPENAI_API_KEY not set in environment (.env)")
        let emptyKey = await drain(provider(stub.port, key: "", model: "").chat(messages: oneUser))
        #expect(pyErrorText(emptyKey.error!) == "OPENAI_API_KEY not set in environment (.env)")
        let noEndpoint = await drain(
            provider(stub.port, endpoint: "", model: "").chat(messages: oneUser))
        #expect(
            "\(noEndpoint.error!)"
                == "RuntimeError: responses_backend: no `endpoint` set — this connector does not "
                + "guess a host. Name the full …/v1/responses URL on the agent record (endpoint=...)."
        )
        let noModel = await drain(provider(stub.port, model: "").chat(messages: oneUser))
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

// ── llamacpp_backend ────────────────────────────────────────────────────

@Suite("llama.cpp provider — the HTTP leg")
struct LlamaCppStreamPinTests {

    private func provider(
        _ endpoint: String, model: String? = nil, key: String? = nil, maxTokens: Int64? = nil,
        think: Bool? = nil
    ) -> LlamaCppProvider {
        LlamaCppProvider(
            endpoint: endpoint, requestedModel: model, apiKey: key, maxTokens: maxTokens,
            temperature: 0.3, think: think)
    }

    @Test func aNon2xxIsHttpNWithNoRetry() async {
        let stub = StubHTTP([.init(status: 500, body: "boom"), .init(status: 200, body: "")])
        defer { stub.stop() }
        let (_, error) = await drain(
            provider("http://127.0.0.1:\(stub.port)/v1").chat(messages: oneUser))
        #expect("\(error!)" == "HTTP 500")
        #expect(pyErrorText(error!) == "HTTP 500")
        #expect(error is LlamaCppError)
        #expect(stub.requests.count == 1)
    }

    @Test func aTurnReportsUsageAndClosesACutThought() async {
        let stub = StubHTTP([
            sse([
                #"{"choices":[{"delta":{"reasoning_content":"r"}}]}"#,
                #"{"choices":[{"delta":{"content":"c"},"finish_reason":"length"}]}"#,
                #"{"choices":[],"usage":{"prompt_tokens":321}}"#,
                "[DONE]",
            ]),
            sse([#"{"choices":[{"delta":{"reasoning_content":"cut"}}]}"#]),
        ])
        defer { stub.stop() }
        let p = provider("http://127.0.0.1:\(stub.port)/v1")
        let (text, error) = await drain(p.chat(messages: oneUser))
        #expect(error == nil && text == "<think>r</think>c")
        #expect(p.usage?.lastPromptTokens == 321)
        #expect(p.usage?.lastStopReason == "max_tokens")
        let (cut, cutError) = await drain(p.chat(messages: oneUser))
        #expect(cutError == nil && cut == "<think>cut</think>")
    }

    /// python's body order — messages, stream, stream_options, model (only when
    /// the record names one), temperature (always), max_tokens, then the
    /// thinking kwarg; every trailing slash of the endpoint is stripped; the key
    /// rides as a Bearer header only when non-empty.
    @Test func theRequestBodyAndHeaders() async {
        let done = sse(["[DONE]"])
        let stub = StubHTTP([done, done, done])
        defer { stub.stop() }
        let base = "http://127.0.0.1:\(stub.port)/v1///"
        _ = await drain(provider(base, key: "").chat(messages: oneUser))
        _ = await drain(
            provider(base, model: "qwen", key: "k-1", maxTokens: 500, think: false).chat(
                messages: oneUser))
        _ = await drain(
            provider(base, maxTokens: 500, think: true).chat(messages: oneUser, maxTokens: 64))
        let msgs =
            #"{"messages":[{"role":"user","content":"hi"}],"stream":true,"stream_options":{"include_usage":true}"#
        #expect(bodyText(stub, 0) == msgs + #","temperature":0.3}"#)
        #expect(
            bodyText(stub, 1)
                == msgs
                + #","model":"qwen","temperature":0.3,"max_tokens":500,"chat_template_kwargs":{"enable_thinking":false}}"#
        )
        #expect(
            bodyText(stub, 2)
                == msgs
                + #","temperature":0.3,"max_tokens":64,"chat_template_kwargs":{"enable_thinking":true}}"#
        )
        #expect(head(stub, 0).hasPrefix("post /v1/chat/completions http/1.1"), "\(head(stub, 0))")
        #expect(!head(stub, 0).contains("authorization"))
        #expect(head(stub, 1).contains("authorization: bearer k-1"))
        #expect(head(stub, 1).contains("accept: text/event-stream"))
    }
}

// ── nvidia_nim_backend ──────────────────────────────────────────────────

@Suite("NIM provider — the HTTP leg")
struct NimStreamPinTests {

    @Test func theRequestBodyHeadersAndUsage() async {
        let stub = StubHTTP([
            sse([
                #"{"choices":[{"delta":{"content":"ok"},"finish_reason":"stop"}]}"#,
                #"{"choices":[],"usage":{"prompt_tokens":12}}"#,
                "[DONE]",
            ]),
            sse(["[DONE]"]),
        ])
        defer { stub.stop() }
        let p = NvidiaNimProvider(
            endpoint: "http://127.0.0.1:\(stub.port)/v1//", model: "m", apiKey: "nv-1")
        let (text, error) = await drain(p.chat(messages: oneUser))
        #expect(error == nil && text == "ok")
        #expect(p.usage?.lastPromptTokens == 12)
        #expect(p.usage?.lastStopReason == "stop")
        _ = await drain(p.chat(messages: oneUser, maxTokens: 64))
        let msgs =
            #"{"model":"m","messages":[{"role":"user","content":"hi"}],"stream":true,"stream_options":{"include_usage":true}"#
        #expect(bodyText(stub, 0) == msgs + "}")
        #expect(bodyText(stub, 1) == msgs + #","max_tokens":64}"#)
        let h = head(stub)
        #expect(h.hasPrefix("post /v1/chat/completions http/1.1"), "\(h)")
        #expect(h.contains("authorization: bearer nv-1"))
        #expect(h.contains("accept: text/event-stream"))
    }

    /// A stream cut mid-reasoning (no `[DONE]`) still closes its think span and
    /// is not an error — NIM's provider checks no terminality.
    @Test func aCutStreamClosesTheThinkSpan() async {
        let stub = StubHTTP([sse([#"{"choices":[{"delta":{"reasoning_content":"half"}}]}"#])])
        defer { stub.stop() }
        let p = NvidiaNimProvider(
            endpoint: "http://127.0.0.1:\(stub.port)/v1", model: "m", apiKey: "k")
        let (text, error) = await drain(p.chat(messages: oneUser))
        #expect(error == nil && text == "<think>half</think>")
    }

    /// A mid-stream failure (here: a non-429 status) is never retried by the
    /// rate-limit wrapper, and the wrapper passes the provider's tokens through.
    @Test func theRetryWrapperPassesANon429Through() async {
        let stub = StubHTTP([.init(status: 500, body: "x"), .init(status: 200, body: "")])
        defer { stub.stop() }
        let p = NvidiaNimProvider(
            endpoint: "http://127.0.0.1:\(stub.port)/v1", model: "m", apiKey: "k")
        let (_, error) = await drain(
            nimRateLimitRetry(provider: p, messages: oneUser, emit: AIStatusEmitter { _, _ in }))
        #expect("\(error!)" == "send: provider HTTP 500")
        #expect(stub.requests.count == 1)
    }
}

// ── anthropic_backend ───────────────────────────────────────────────────

@Suite("Anthropic provider — the HTTP leg")
struct AnthropicStreamPinTests {

    private func provider(_ port: Int) -> AnthropicProvider {
        AnthropicProvider(
            endpoint: "http://127.0.0.1:\(port)/v1/messages", model: "claude-x", apiKey: "k-ant",
            maxTokens: 64, sampling: [:])
    }

    @Test func aTurnReportsUsageAndSendsTheHeaders() async {
        let stub = StubHTTP([
            sse([
                #"{"type":"message_start","message":{"usage":{"input_tokens":10,"cache_read_input_tokens":5}}}"#,
                #"{"type":"content_block_start","content_block":{"type":"text"}}"#,
                #"{"type":"content_block_delta","delta":{"type":"text_delta","text":"hi"}}"#,
                #"{"type":"message_delta","delta":{"stop_reason":"end_turn"}}"#,
                #"{"type":"message_stop"}"#,
            ])
        ])
        defer { stub.stop() }
        let p = provider(stub.port)
        let (text, error) = await drain(p.chat(messages: withSystem))
        #expect(error == nil && text == "hi")
        #expect(p.usage?.lastPromptTokens == 15)
        #expect(p.usage?.lastStopReason == "end_turn")
        let h = head(stub)
        #expect(h.hasPrefix("post /v1/messages http/1.1"))
        #expect(h.contains("x-api-key: k-ant"))
        #expect(h.contains("anthropic-version: "))
        #expect(h.contains("accept: text/event-stream"))
        #expect(bodyText(stub) == JSON.object(p.requestBody(messages: withSystem)).serialize())
    }

    /// An `error` event ends the turn AT ONCE — the half-open think span is not
    /// closed on this path (python raises from inside the loop).
    @Test func anErrorEventMidThoughtRaisesWithoutClosingTheSpan() async {
        let stub = StubHTTP([
            sse([
                #"{"type":"content_block_start","content_block":{"type":"thinking"}}"#,
                #"{"type":"content_block_delta","delta":{"type":"thinking_delta","thinking":"half"}}"#,
                #"{"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#,
            ])
        ])
        defer { stub.stop() }
        let (text, error) = await drain(provider(stub.port).chat(messages: oneUser))
        #expect(text == "<think>half")
        #expect(
            "\(error!)"
                == "RuntimeError: anthropic stream error: {'type': 'overloaded_error', 'message': 'Overloaded'}"
        )
    }

    /// A bounded side call that stopped at `max_tokens` is an ANSWER; the same
    /// stream on a main turn is the truncation error.
    @Test func aMaxTokensStopIsAnAnswerOnlyOnASideCall() async {
        let cut = sse([
            #"{"type":"content_block_start","content_block":{"type":"text"}}"#,
            #"{"type":"content_block_delta","delta":{"type":"text_delta","text":"part"}}"#,
            #"{"type":"message_delta","delta":{"stop_reason":"max_tokens"}}"#,
            #"{"type":"message_stop"}"#,
        ])
        let stub = StubHTTP([cut, cut])
        defer { stub.stop() }
        let side = provider(stub.port)
        let (text, sideError) = await drain(side.chat(messages: oneUser, maxTokens: 32))
        #expect(sideError == nil && text == "part")
        #expect(side.usage?.lastStopReason == "max_tokens")
        #expect(bodyText(stub, 0).contains(#""max_tokens":32"#))
        let (_, mainError) = await drain(provider(stub.port).chat(messages: oneUser))
        #expect(
            pyErrorText(mainError!)
                == "anthropic stream ended without end_turn: stop_reason='max_tokens' blocks=['text']"
        )
    }

    @Test func aGatewayPageWithNoContentTypeLogsTheRetryLine() async {
        let stub = StubHTTP([
            .init(status: 504, body: "timeout"),
            sse([
                #"{"type":"content_block_start","content_block":{"type":"text"}}"#,
                #"{"type":"content_block_delta","delta":{"type":"text_delta","text":"ok"}}"#,
                #"{"type":"message_delta","delta":{"stop_reason":"end_turn"}}"#,
            ]),
        ])
        defer { stub.stop() }
        var result: (text: String, error: Error?) = ("", nil)
        let lines = await RuntimeLog.captureOut {
            result = await drain(provider(stub.port).chat(messages: oneUser))
        }
        #expect(result.error == nil && result.text == "ok")
        #expect(
            lines.contains("[anthropic] 504 (no content-type) — transport error, retry 1/2"),
            "\(lines)")
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
            model: "q", maxTokens: 8000, kvSlots: 7, kvOverflow: "evict", kvTtl: KV_TTL,
            sampling: SAMPLING_DEFAULTS, explicitBudget: false,
            connectTimeout: CONNECT_TIMEOUT, readTimeout: READ_TIMEOUT,
            agentKey: UUID().uuidString, clock: { ProcessInfo.processInfo.systemUptime },
            apiKey: { nil })
        let (text, error) = await drain(p.chat(messages: oneUser))
        #expect(text == "a")
        #expect((error as? PyError)?.type == "JSONDecodeError", "\(String(describing: error))")
        #expect("\(error!)".hasPrefix("JSONDecodeError: "))
    }
}
