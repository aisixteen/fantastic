// What each provider does at the wire, pinned against python's own policy.
//
// These are the B4 items: which status codes retry (and which emphatically do
// not), what a failure SAYS, which URL the record's `endpoint` builds, and which
// fields always ride the body. The stub (`StubHTTP`) is a loopback listener —
// python reaches the same places with an httpx transport seam.

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

/// Drain a provider stream, returning the tokens it yielded and the error it
/// ended with (if any) — a failing turn's TOKENS matter as much as its error.
private func drain(_ stream: AsyncThrowingStream<AIChunk, Error>) async -> (
    tokens: [String], error: Error?
) {
    var tokens: [String] = []
    do {
        for try await chunk in stream {
            if case .token(let t) = chunk { tokens.append(t) }
        }
        return (tokens, nil)
    } catch {
        return (tokens, error)
    }
}

private let oneUser: [JSON] = [.object(["role": .string("user"), "content": .string("hi")])]

@Suite("Provider policy")
struct ProviderPolicyTests {

    private func anthropic(port: Int, model: String = "claude-x", key: String? = "k")
        -> AnthropicProvider
    {
        AnthropicProvider(
            endpoint: "http://127.0.0.1:\(port)/v1/messages", model: model, apiKey: key,
            maxTokens: 64, sampling: [:])
    }

    /// One complete Anthropic turn: text, then a terminal `end_turn`.
    private let goodTurn = """
        data: {"type":"content_block_start","content_block":{"type":"text"}}

        data: {"type":"content_block_delta","delta":{"type":"text_delta","text":"hi"}}

        data: {"type":"message_delta","delta":{"stop_reason":"end_turn"}}

        data: {"type":"message_stop"}


        """

    // ── B4.1: the anthropic status policy ────────────────────────────────

    /// A 4xx is the endpoint answering DELIBERATELY: `anthropic {code}: {body}`,
    /// raised, with the body the caller needs to read. Swift threw a bare
    /// `send: provider HTTP 400` and dropped the one sentence naming the cause.
    @Test func anthropicStatusErrorNamesTheCodeAndTheBody() async {
        let stub = StubHTTP([
            .init(
                status: 400, headers: [("Content-Type", "application/json")],
                body: #"{"error":"bad model"}"#)
        ])
        defer { stub.stop() }
        let (_, error) = await drain(anthropic(port: stub.port).chat(messages: oneUser))
        #expect(pyErrorText(error!) == #"anthropic 400: {"error":"bad model"}"#)
        // …and it RAISES: the settle a caller reads says `RuntimeError`.
        #expect("\(error!)" == #"RuntimeError: anthropic 400: {"error":"bad model"}"#)
        #expect(stub.requests.count == 1)
    }

    /// NO 429 RETRY, and no event for one. python's anthropic provider has no
    /// rate-limit branch at all — the endpoint answered deliberately, and
    /// sleeping inside the turn burns the send ceiling while the caller watches
    /// a `thinking` phase that never ends.
    @Test func anthropicNeverRetriesA429() async {
        let stub = StubHTTP([
            .init(
                status: 429, headers: [("Content-Type", "application/json"), ("retry-after", "1")],
                body: "slow down"),
            .sse(goodTurn),
        ])
        defer { stub.stop() }
        let (_, error) = await drain(anthropic(port: stub.port).chat(messages: oneUser))
        #expect(pyErrorText(error!) == "anthropic 429: slow down")
        #expect(stub.requests.count == 1, "a 429 is an answer, not a hiccup")
    }

    /// A transport code with a NON-JSON body is a CDN error page: the request
    /// never reached anything that could judge it, so it is retried.
    @Test func aTransportCodeWithANonJsonBodyIsRetried() async {
        let stub = StubHTTP([
            .init(
                status: 503, headers: [("Content-Type", "text/html")], body: "<html>gateway</html>"),
            .sse(goodTurn),
        ])
        defer { stub.stop() }
        var tokens: [String] = []
        var error: Error?
        // A15.4: before each transport retry, python prints
        // `[anthropic] {code} ({ctype}) — transport error, retry n/2` on
        // stdout — neither port did.
        let lines = await RuntimeLog.captureOut {
            (tokens, error) = await drain(anthropic(port: stub.port).chat(messages: oneUser))
        }
        #expect(error == nil)
        #expect(tokens.joined() == "hi")
        #expect(stub.requests.count == 2)
        #expect(
            lines.contains("[anthropic] 503 (text/html) — transport error, retry 1/2"), "\(lines)")
    }

    /// The SAME code with a JSON body is the API speaking: re-sending a real
    /// refusal until the budget ran out is worse than failing loudly.
    @Test func aTransportCodeWithAJsonBodyIsNotRetried() async {
        let stub = StubHTTP([
            .init(
                status: 503, headers: [("Content-Type", "application/json")],
                body: #"{"error":"overloaded"}"#),
            .sse(goodTurn),
        ])
        defer { stub.stop() }
        let (_, error) = await drain(anthropic(port: stub.port).chat(messages: oneUser))
        #expect(pyErrorText(error!) == #"anthropic 503: {"error":"overloaded"}"#)
        #expect(stub.requests.count == 1)
    }

    /// And the retry is BOUNDED: two attempts after the first, then the status
    /// is the answer. python `test_transport_error_gives_up_after_the_bound`.
    @Test func transportRetriesGiveUpAfterTheBound() async {
        let page = StubHTTP.Reply(
            status: 502, headers: [("Content-Type", "text/html")], body: "bad gateway")
        let stub = StubHTTP([page, page, page, .sse(goodTurn)])
        defer { stub.stop() }
        let (_, error) = await drain(anthropic(port: stub.port).chat(messages: oneUser))
        #expect(pyErrorText(error!) == "anthropic 502: bad gateway")
        #expect(stub.requests.count == 3, "the first attempt plus RETRIES(2)")
    }

    // ── B4.10: the think span closes at END OF BODY ──────────────────────

    /// A stream cut mid-thought still closes its `<think>` span — BEFORE the
    /// terminality check — so the text a parser reads is always balanced. The
    /// port closed it only on `message_stop`, which is exactly the frame a cut
    /// stream never sends.
    @Test func anOpenThinkSpanIsClosedAtEndOfBody() async {
        let cut = """
            data: {"type":"content_block_start","content_block":{"type":"thinking"}}

            data: {"type":"content_block_delta","delta":{"type":"thinking_delta","thinking":"half a thou"}}


            """
        let stub = StubHTTP([.sse(cut)])
        defer { stub.stop() }
        let (tokens, error) = await drain(anthropic(port: stub.port).chat(messages: oneUser))
        #expect(tokens.joined() == "<think>half a thou</think>")
        #expect(
            pyErrorText(error!)
                == "anthropic stream ended without end_turn: stop_reason=None "
                + "blocks=['thinking'] (none seen — connection cut mid-stream)")
    }

    // ── B4.2: the key is checked INSIDE chat() ───────────────────────────

    /// python reads the variable at call time and raises from inside the turn,
    /// so the caller's settle is `{error:"RuntimeError: …", raised:true}` and the
    /// status event carries the bare message. The key check comes FIRST: an
    /// agent missing both is told about the variable it must export.
    @Test func anthropicMissingKeyRaisesBeforeTheModelCheck() async {
        let (_, error) = await drain(
            anthropic(port: 1, model: "", key: nil)
                .chat(messages: oneUser))
        #expect(pyErrorText(error!) == "ANTHROPIC_API_KEY not set in environment (.env)")
        #expect("\(error!)" == "RuntimeError: ANTHROPIC_API_KEY not set in environment (.env)")
        // An EMPTY variable is no key either.
        let (_, empty) = await drain(
            anthropic(port: 1, key: "").chat(messages: oneUser))
        #expect(pyErrorText(empty!) == "ANTHROPIC_API_KEY not set in environment (.env)")
    }

    // ── B4.3 / B4.7: the nvidia key and endpoint ─────────────────────────

    @Test func nimRefusesWithTheEnvironmentVariableNamed() async {
        let kernel = Kernel(storage: .inMemory, bundles: BundleRegistry())
        let agent = Agent(id: AgentId("nim"), handlerModule: nil, parentId: nil)
        let saved = ProcessInfo.processInfo.environment["NVIDIA_API_KEY"]
        unsetenv("NVIDIA_API_KEY")
        defer { if let saved { setenv("NVIDIA_API_KEY", saved, 1) } }

        let result = try? await NvidiaNimBundle().core.config.makeProvider(agent, "c", kernel)
        guard case .refused(let body)? = result else {
            Issue.record("an unwired connector must refuse")
            return
        }
        #expect(body["error"].asString == "nvidia_nim_backend: no api key — set NVIDIA_API_KEY")
        // The refusal is the SEND's reply, so it carries the caller's id — and
        // nothing else: swift used to answer `{error, reason:"no_api_key"}`.
        #expect(body.asObject.map { Array($0.keys) } == ["error", "client_id"])
        #expect(body["client_id"].asString == "c")
        // `has_api_key` tracks the environment, and the key itself is never in
        // a reflect.
        let bare = NvidiaNimBundle().core.config.reflectExtra(agent)
        #expect(Array(bare.keys) == ["has_api_key"], "no `host` — `endpoint` is the base field")
        #expect(bare["has_api_key"] == JSON.bool(false))
        setenv("NVIDIA_API_KEY", "nvapi-test", 1)
        #expect(NvidiaNimBundle().core.config.reflectExtra(agent)["has_api_key"] == JSON.bool(true))
        unsetenv("NVIDIA_API_KEY")
    }

    /// The record's `endpoint` is the OpenAI-compatible BASE; the provider
    /// appends `/chat/completions`. Swift read a `host` meta and appended
    /// `/v1/chat/completions`, so one runtime's working record 404'd on the other.
    @Test func nimRecordEndpointBuildsChatCompletionsUrl() async {
        let stub = StubHTTP([
            .sse(
                """
                data: {"choices":[{"delta":{"content":"ok"}}]}

                data: [DONE]


                """)
        ])
        defer { stub.stop() }
        let provider = NvidiaNimProvider(
            endpoint: "http://127.0.0.1:\(stub.port)/v1", model: "m", apiKey: "k")
        let (tokens, error) = await drain(provider.chat(messages: oneUser))
        #expect(error == nil)
        #expect(tokens.joined() == "ok")
        #expect(
            stub.requests.first?.hasPrefix("POST /v1/chat/completions HTTP") == true,
            Comment(rawValue: stub.requests.first ?? "no request"))
    }

    // ── B4.8: the nvidia 429 retry ───────────────────────────────────────

    /// ONE retry on a PRE-STREAM 429, after `Retry-After` (clamped), with the
    /// wait surfaced as `status(thinking, waiting_on:'rate_limit', wait_s)`.
    @Test func nimRetriesOnceOnAPreStreamRateLimit() async {
        let stub = StubHTTP([
            .init(status: 429, headers: [("retry-after", "1")], body: "slow down"),
            .sse(
                """
                data: {"choices":[{"delta":{"content":"second try"}}]}

                data: [DONE]


                """),
        ])
        defer { stub.stop() }
        let provider = NvidiaNimProvider(
            endpoint: "http://127.0.0.1:\(stub.port)/v1", model: "m", apiKey: "k")
        let emitted = EmittedStatuses()
        let stream = nimRateLimitRetry(
            provider: provider, messages: oneUser,
            emit: AIStatusEmitter { phase, detail in emitted.add(phase, detail) })
        let (tokens, error) = await drain(stream)
        #expect(error == nil)
        #expect(tokens.joined() == "second try", "never an empty success")
        #expect(stub.requests.count == 2)
        #expect(emitted.phases() == ["thinking"])
        #expect(emitted.keys() == [["waiting_on", "wait_s"]])
        #expect(emitted.detail(0)["wait_s"] == .integer(1))
        #expect(emitted.detail(0)["waiting_on"] == .string("rate_limit"))
    }

    /// A SECOND 429 is the answer: the mapper turns it into python's sentence,
    /// which the backend returns as `{error, client_id}` (B2.1's mapped half).
    @Test func nimSurfacesTheErrorAfterARepeatedRateLimit() async {
        let limited = StubHTTP.Reply(status: 429, headers: [("retry-after", "1")], body: "no")
        let stub = StubHTTP([limited, limited])
        defer { stub.stop() }
        let provider = NvidiaNimProvider(
            endpoint: "http://127.0.0.1:\(stub.port)/v1", model: "m", apiKey: "k")
        let emitted = EmittedStatuses()
        let (_, error) = await drain(
            nimRateLimitRetry(
                provider: provider, messages: oneUser,
                emit: AIStatusEmitter { phase, detail in emitted.add(phase, detail) }))
        let mapped = NvidiaNimBundle().core.config.errorMapper?(error!)
        #expect(mapped == "send: rate limited (429); retry in 1s")
        #expect(stub.requests.count == 2, "one retry, not two")
    }

    @Test func nimMapsAPlainHttpStatus() async {
        let stub = StubHTTP([.init(status: 500, body: "boom")])
        defer { stub.stop() }
        let provider = NvidiaNimProvider(
            endpoint: "http://127.0.0.1:\(stub.port)/v1", model: "m", apiKey: "k")
        let (_, error) = await drain(provider.chat(messages: oneUser))
        #expect(NvidiaNimBundle().core.config.errorMapper?(error!) == "send: provider HTTP 500")
    }

    /// `Retry-After` is clamped to [1, 60]; anything unparseable is the default.
    @Test func parseRetryAfterClampsAndDefaults() {
        #expect(parseRetryAfter("30") == 30)
        #expect(parseRetryAfter("0") == 1)
        #expect(parseRetryAfter("-5") == 1)
        #expect(parseRetryAfter("9999") == 60)
        #expect(parseRetryAfter(nil) == 5)
        #expect(parseRetryAfter("soon") == 5)
        #expect(parseRetryAfter("3.5") == 5, "python's `int()` refuses a float spelling")
    }

    // ── B4.4: llamacpp always sends `temperature` ────────────────────────

    /// python defaults it to 0.3 at construction and writes
    /// `if self._temperature is not None`, which is never false. Swift carried
    /// the record's optional straight through, so an agent with no temperature
    /// meta ran at llama-server's own (much higher) default while its reflect
    /// reported 0.3.
    @Test func llamacppAlwaysSendsTemperature() async {
        let stub = StubHTTP([
            .sse(
                """
                data: {"choices":[{"delta":{"content":"ok"}}]}

                data: [DONE]


                """)
        ])
        defer { stub.stop() }
        let bundle = LlamaCppBundle()
        let agent = Agent(
            id: AgentId("llm"), handlerModule: nil, parentId: nil,
            meta: ["endpoint": .string("http://127.0.0.1:\(stub.port)/v1")])
        let kernel = Kernel(storage: .inMemory, bundles: BundleRegistry())
        guard
            case .provider(let provider)? = try? await bundle.core.config.makeProvider(agent, "c", kernel)
        else {
            Issue.record("llamacpp never refuses a provider")
            return
        }
        _ = await drain(provider.chat(messages: oneUser))
        let body = stub.requests.first?.components(separatedBy: "\r\n\r\n").last ?? ""
        #expect(
            body.contains("\"temperature\":0.3"), Comment(rawValue: stub.requests.first ?? "none"))
    }

    // ── B4.6: the ollama model and endpoint ──────────────────────────────

    /// An unset model FAILS LOUD when the provider is built, naming the field —
    /// python's `required_meta`.
    @Test func ollamaUnsetModelFailsLoudWithPythonText() async {
        let bundle = OllamaBackendBundle()
        let agent = Agent(
            id: AgentId("ol"), handlerModule: nil, parentId: nil,
            meta: ["endpoint": .string("http://127.0.0.1:9")])
        let kernel = Kernel(storage: .inMemory, bundles: BundleRegistry())
        do {
            _ = try await bundle.core.config.makeProvider(agent, "c", kernel)
            Issue.record("an unset model must refuse the provider")
        } catch {
            #expect(
                "\(error)"
                    == "ValueError: ollama_backend: `model` is required on the agent record — "
                    + "nothing is guessed (update_agent <id> model=…)")
        }
    }

    /// The record's `endpoint` is where ollama listens; `/api/chat` is appended.
    /// Swift read a `host` meta and ignored `endpoint` entirely.
    @Test func ollamaRecordEndpointBuildsApiChatUrl() async {
        let stub = StubHTTP([.init(body: #"{"message":{"content":"ok"},"done":true}"#)])
        defer { stub.stop() }
        let bundle = OllamaBackendBundle()
        let agent = Agent(
            id: AgentId("ol"), handlerModule: nil, parentId: nil,
            meta: ["model": .string("m"), "endpoint": .string("http://127.0.0.1:\(stub.port)")])
        let kernel = Kernel(storage: .inMemory, bundles: BundleRegistry())
        guard
            case .provider(let provider)? = try? await bundle.core.config.makeProvider(agent, "c", kernel)
        else {
            Issue.record("unexpected refusal")
            return
        }
        _ = await drain(provider.chat(messages: oneUser))
        #expect(
            stub.requests.first?.hasPrefix("POST /api/chat HTTP") == true,
            Comment(rawValue: stub.requests.first ?? "no request"))
    }
}

/// The `status` events a stream wrapper emitted, in order.
private final class EmittedStatuses: @unchecked Sendable {
    private let lock = NSLock()
    private var seen: [(String, [(String, JSON)])] = []

    func add(_ phase: String, _ detail: [(String, JSON)]) {
        lock.lock()
        seen.append((phase, detail))
        lock.unlock()
    }

    func phases() -> [String] {
        lock.lock()
        defer { lock.unlock() }
        return seen.map { $0.0 }
    }

    func keys() -> [[String]] {
        lock.lock()
        defer { lock.unlock() }
        return seen.map { $0.1.map { $0.0 } }
    }

    func detail(_ i: Int) -> [String: JSON] {
        lock.lock()
        defer { lock.unlock() }
        return Dictionary(uniqueKeysWithValues: seen[i].1)
    }
}
