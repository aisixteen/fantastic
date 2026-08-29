// Anthropic Messages-API LLM backend.
//
// Mirrors Rust's `fantastic-anthropic-backend::AnthropicBundle` and python's
// `anthropic_backend`. Talks to the Messages API via HTTPS POST + `x-api-key` +
// `anthropic-version`; streams RAW TEXT through event-typed Server-Sent Events.
//
// NO NATIVE TOOL-CALLING. Tool-calling here is raw prompt-and-parse owned by
// FantasticAICore: the provider streams text and the core parses the
// `<tool_call>` envelope out of it. The optional `tool_use` route declares the
// ONE `send` tool and TRANSLATES what comes back into that same envelope — it
// changes what we ASK for, never what we can read.
//
// All the agent machinery (history, epoch cancellation, verb dispatch,
// token/done events, envelope parsing) lives in the shared
// `FantasticAICore.AIBackend`. This file keeps only Anthropic-specific wire.

import AsyncHTTPClient
import FantasticAICore
import FantasticJSON
import FantasticKernel
import Foundation
import NIOCore
import OrderedCollections

public let HANDLER_MODULE = "anthropic_backend.tools"

/// The API's own default host. An `endpoint` on the record overrides it — and
/// MUST, for any proxy: without it the record falls back here while carrying
/// whatever key the run exported, which is a 401 the moment that key belongs to
/// a different host.
public let DEFAULT_ENDPOINT = "https://api.anthropic.com/v1/messages"

/// The variables that carry the key, in python's order (`provider.py`).
/// Named in the failure text so an unwired connector says what to set.
let KEY_VARS = ["ANTHROPIC_KEY", "ANTHROPIC_API_KEY"]

/// Read the key from the environment. Nothing else is consulted: not the agent
/// record, not a sidecar, not settings.
func readApiKey() -> String? {
    for v in KEY_VARS {
        if let raw = ProcessInfo.processInfo.environment[v] {
            let t = raw.trimmingCharacters(in: .whitespacesAndNewlines)
            if !t.isEmpty { return t }
        }
    }
    return nil
}

/// The refusal for an unwired connector — names the variables to set.
func noKeyError() -> String {
    "anthropic_backend: no api key — set \(KEY_VARS.joined(separator: " or "))"
}
public let DEFAULT_MODEL = "claude-sonnet-4-6"
public let ANTHROPIC_VERSION = "2023-06-01"
/// `max_tokens` is REQUIRED by the Messages API — defaulted when unset.
public let DEFAULT_MAX_TOKENS = 8192

public final class AnthropicBundle: AgentBundle, @unchecked Sendable {
    public let name = "anthropic_backend"

    private let core: AIBackend

    public init() {
        self.core = buildAIBackend(
            AIBackendConfig(
                kind: "anthropic_backend",
                provider: "anthropic",
                sentence:
                    "Anthropic-backed LLM agent (Messages API; raw prompt-and-parse tool-calling).",
                verbs: [
                    "send": "args: text, client_id?. Streams a response via SSE.",
                    "history": "args: client_id?. Returns prior turns.",
                    "cancel": .string(CANCEL_DOC),
                    "status":
                        "args: client_id?. In-flight phase + this client's pending queue (others' text redacted).",
                    "backend_state": "Reports availability + in-flight.",
                ] as JSON,
                includeAccumulatedOnError: true,
                reflectExtra: { agent in
                    [
                        "endpoint": .string(Self.endpoint(agent: agent)),
                        "model": .string(Self.model(agent: agent)),
                    ]
                },
                backendStateExtra: { agent in
                    [
                        "endpoint": .string(Self.endpoint(agent: agent)),
                        "model": .string(Self.model(agent: agent)),
                        "configured": .bool(readApiKey() != nil),
                    ]
                },
                makeProvider: { agent, _, _ in
                    // THE ENVIRONMENT IS THE ONLY SOURCE — python-canonical.
                    // The key used to be read off the agent's own meta, which
                    // means it was WRITTEN INTO agent.json on disk; rust and
                    // python's nvidia bundle both kept it out of the record for
                    // exactly that reason. Now nothing persists it at all.
                    guard let apiKey = readApiKey() else {
                        return .refused(
                            .object([
                                "error": .string(noKeyError()),
                                "reason": .string("no_api_key"),
                            ]))
                    }
                    return .provider(
                        AnthropicProvider(
                            endpoint: Self.endpoint(agent: agent),
                            model: Self.model(agent: agent),
                            apiKey: apiKey,
                            maxTokens: agent.metaValue(forKey: "max_tokens")?.asInt
                                ?? Int64(DEFAULT_MAX_TOKENS),
                            toolUse: agent.metaValue(forKey: "tool_use")?.asBool ?? false))
                }
            ))
    }

    public var readme: String? {
        """
        anthropic_backend — Anthropic Messages-API LLM agent; thin over FantasticAICore.
        Verbs: send, history, cancel, status, backend_state; the api key comes from the ENVIRONMENT (ANTHROPIC_KEY, else ANTHROPIC_API_KEY) and is never stored; 429 + gateway retry.
        Tool-calling is RAW: the provider streams text and FantasticAICore parses the <tool_call>/<tool_response> envelope (tool_parse).
        `tool_use: true` declares the one `send` tool and TRANSLATES the returned blocks into that same envelope — it changes what is asked for, not what can be read.
        `endpoint` overrides the API host; set it for any proxy, or the default host is used with your key and answers 401.
        """
    }

    public func handle(
        agentId: AgentId,
        payload: JSON,
        kernel: Kernel
    ) async throws -> JSON? {
        // NO key-storage verbs: the key is read from the ENVIRONMENT at call
        // time, as in python and rust. A connector stays inert until the
        // operator wires the variable.
        return await core.handle(agentId: agentId, payload: payload, kernel: kernel)
    }

    /// The kernel is killing a task of this agent — close the turn out for the
    /// caller before the substrate cancels it.
    public func onCancel(agentId: AgentId, taskId: TaskId, kernel: Kernel) async throws {
        await core.onCancel(agentId: agentId, taskId: taskId, kernel: kernel)
    }

    // MARK: - meta helpers

    fileprivate static func endpoint(agent: Agent) -> String {
        agent.metaValue(forKey: "endpoint")?.asString ?? DEFAULT_ENDPOINT
    }

    fileprivate static func model(agent: Agent) -> String {
        agent.metaValue(forKey: "model")?.asString ?? DEFAULT_MODEL
    }
}

/// The ONE tool `ai_core` teaches in prose. Sent ONLY on the `tool_use` route.
/// Its shape must match what the core's parser already expects, because what
/// comes back is translated into exactly that envelope.
func anthropicSendTool() -> JSON {
    .object([
        "name": .string("send"),
        "description": .string(
            "Send a payload to another agent by id and get its reply."),
        "input_schema": .object([
            "type": .string("object"),
            "properties": .object([
                "target_id": .object(["type": .string("string")]),
                "payload": .object(["type": .string("object")]),
            ]),
            "required": .array([.string("target_id"), .string("payload")]),
        ]),
    ])
}

/// Messages-API SSE provider — `x-api-key` auth, 429 retry with `retry-after`,
/// and a bounded retry for transient GATEWAY failures.
struct AnthropicProvider: AIProvider {
    let endpoint: String
    let model: String
    let apiKey: String
    let maxTokens: Int64
    let toolUse: Bool

    /// 502/503/504/520-524 from a CDN in front of the API is a hiccup, not an
    /// answer: a 1-2s restart once returned a Cloudflare HTML page and killed a
    /// 26-pass run at pass 5. Retried ONLY when the body is not JSON — a JSON
    /// body is the API speaking deliberately, and re-sending a real refusal
    /// until the budget ran out would be worse than failing loudly.
    static let gatewayMaxRetries = 2
    static let rateLimitMaxRetries = 1

    func chat(messages: [JSON]) -> AsyncThrowingStream<AIChunk, Error> {
        let endpoint = endpoint
        let model = model
        let apiKey = apiKey
        let maxTokens = maxTokens
        let toolUse = toolUse
        return AsyncThrowingStream { continuation in
            let task = Task {
                let (system, turns) = translateAnthropicMessages(messages)
                var body: OrderedDictionary<String, JSON> = [:]
                body["model"] = .string(model)
                body["max_tokens"] = .integer(maxTokens)
                body["stream"] = .bool(true)
                body["messages"] = .array(turns)
                if let system { body["system"] = .string(system) }
                if toolUse { body["tools"] = .array([anthropicSendTool()]) }
                let payload = JSON.object(body).serialize()

                // VERBATIM — `endpoint` IS the messages URL, as in python
                // (see DEFAULT_ENDPOINT). It used to be treated as a base with
                // `/messages` appended, so one meta key meant two things across
                // runtimes and a working proxy URL 404'd on the other kernel.
                let url = endpoint

                var rateAttempt = 0
                var gatewayAttempt = 0

                while true {
                    var req = HTTPClientRequest(url: url)
                    req.method = .POST
                    req.headers.add(name: "Content-Type", value: "application/json")
                    req.headers.add(name: "x-api-key", value: apiKey)
                    req.headers.add(name: "anthropic-version", value: ANTHROPIC_VERSION)
                    req.headers.add(name: "Accept", value: "text/event-stream")
                    req.body = .bytes(ByteBuffer(string: payload))

                    do {
                        let response = try await HTTPClient.shared.execute(
                            req, timeout: .seconds(600))
                        let code = Int(response.status.code)

                        if code == 429, rateAttempt < Self.rateLimitMaxRetries {
                            rateAttempt += 1
                            let wait = response.headers.first(name: "retry-after")
                                .flatMap(Int.init) ?? 5
                            try? await Task.sleep(
                                nanoseconds: UInt64(min(wait, 60)) * 1_000_000_000)
                            continue
                        }
                        let isGateway =
                            code == 502 || code == 503 || code == 504 || (520...524).contains(code)
                        if isGateway, gatewayAttempt < Self.gatewayMaxRetries {
                            let bodyText = try? await response.body.collect(upTo: 1 << 16)
                            let raw = bodyText.map { String(buffer: $0) } ?? ""
                            if (try? JSON.parse(Data(raw.utf8))) == nil {
                                let wait = UInt64(1 << gatewayAttempt)  // 1s, 2s
                                gatewayAttempt += 1
                                try? await Task.sleep(nanoseconds: wait * 1_000_000_000)
                                continue
                            }
                            continuation.finish(throwing: AnthropicError.http(code))
                            return
                        }
                        if code >= 400 {
                            continuation.finish(throwing: AnthropicError.http(code))
                            return
                        }

                        var state = AnthropicStreamState()
                        var streamError: String? = nil
                        for try await line in bytesToLines(response.body) {
                            let d = decodeAnthropicSSELine(line, state: &state)
                            for token in d.tokens { continuation.yield(.token(token)) }
                            if let err = d.error {
                                streamError = err
                                break
                            }
                            if d.done { break }
                        }
                        // The model spent the tail of its turn reasoning: close
                        // the span so the text a parser reads stays balanced.
                        for token in state.wrap.finish() { continuation.yield(.token(token)) }

                        if let streamError {
                            continuation.finish(throwing: AnthropicError.stream(streamError))
                            return
                        }
                        if let why = anthropicTerminalityError(state) {
                            continuation.finish(throwing: AnthropicError.stream(why))
                            return
                        }
                        continuation.finish()
                        return
                    } catch {
                        continuation.finish(throwing: error)
                        return
                    }
                }
            }
            continuation.onTermination = { _ in task.cancel() }
        }
    }
}

/// Failures that carry their own text, matching rust's error strings.
enum AnthropicError: Error, CustomStringConvertible {
    case http(Int)
    case stream(String)
    var description: String {
        switch self {
        case .http(let code): return "send: provider HTTP \(code)"
        case .stream(let msg): return msg
        }
    }
}
