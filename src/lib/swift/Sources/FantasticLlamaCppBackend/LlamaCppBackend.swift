// `llama-server` (llama.cpp) LLM backend.
//
// Mirrors python's `llamacpp_backend`. llama-server speaks the
// OpenAI-compatible surface at `<endpoint>/chat/completions` and streams RAW
// TEXT via SSE, so the wire decode is the SHARED
// `FantasticAICore.decodeOpenAISSELine` — the same one NIM uses.
//
// NO NATIVE TOOL-CALLING: the provider streams text and FantasticAICore parses
// the `<call name=…>` envelope out of it.
//
// Two things this backend does NOT pretend to own:
//
//   * THE MODEL. llama-server serves exactly one (whichever `-hf`/`-m` loaded)
//     and ignores the request's `model` field, so nothing is sent unless the
//     record names one.
//   * THE CONTEXT WINDOW. Fixed at server start (`-c N`); there is no
//     per-request equivalent, so set it on the llama-server command line.

import AsyncHTTPClient
import FantasticAICore
import FantasticJSON
import FantasticKernel
import Foundation
import NIOCore
import OrderedCollections

public let HANDLER_MODULE = "llamacpp_backend.tools"

/// Where a local llama-server listens by default.
public let DEFAULT_ENDPOINT = "http://127.0.0.1:8080/v1"

public final class LlamaCppBundle: AgentBundle, @unchecked Sendable {
    public let name = "llamacpp_backend"

    private let core: AIBackend

    public init() {
        self.core = buildAIBackend(
            AIBackendConfig(
                kind: "llamacpp_backend",
                provider: "llamacpp",
                sentence:
                    "llama.cpp-backed local LLM agent (raw prompt-and-parse tool-calling).",
                includeAccumulatedOnError: true,
                reflectExtra: { agent in
                    ["endpoint": .string(Self.endpoint(agent: agent))]
                },
                makeProvider: { agent, _, _ in
                    .provider(
                        LlamaCppProvider(
                            endpoint: Self.endpoint(agent: agent),
                            requestedModel: agent.metaValue(forKey: "model")?.asString,
                            apiKey: agent.metaValue(forKey: "api_key")?.asString,
                            maxTokens: agent.metaValue(forKey: "num_predict")?.asInt,
                            temperature: agent.metaValue(forKey: "temperature")?.asDouble,
                            think: agent.metaValue(forKey: "think")?.asBool))
                }
            ))
    }

    public var readme: String? {
        """
        llamacpp_backend — local llama-server LLM agent (OpenAI-compatible); thin over FantasticAICore.
        Verbs: the shared ai_core set (reflect it). `endpoint` defaults to http://127.0.0.1:8080/v1.
        Tool-calling is RAW: the provider streams text and FantasticAICore parses the <call name=…> envelope.
        `think` toggles the template's `enable_thinking` per request — gemma-4 gates its thought channel behind it and defaults it FALSE; a server that sets neither streams thinking text with EMPTY content.
        The model and the context window belong to the server (`-hf`/`-m`, `-c N`), not to the record.
        """
    }

    public func handle(agentId: AgentId, payload: JSON, kernel: Kernel) async throws -> JSON? {
        await core.handle(agentId: agentId, payload: payload, kernel: kernel)
    }

    public func onCancel(agentId: AgentId, taskId: TaskId, kernel: Kernel) async throws {
        await core.onCancel(agentId: agentId, taskId: taskId, kernel: kernel)
    }

    fileprivate static func endpoint(agent: Agent) -> String {
        agent.metaValue(forKey: "endpoint")?.asString ?? DEFAULT_ENDPOINT
    }
}

/// llama-server `/chat/completions` SSE provider. PURE RAW TEXT: yields
/// `.token`s decoded from BOTH channels through the shared OpenAI decode.
struct LlamaCppProvider: AIProvider {
    let endpoint: String
    /// What the RECORD asked for, if anything. llama-server serves exactly one
    /// model and ignores this field, so it is usually nil and nothing is sent.
    let requestedModel: String?

    /// The protocol wants a model id. Reporting a guess here would be a lie the
    /// reflect surface then repeats, so an unset one says whose choice it is.
    var model: String { requestedModel ?? "(llama-server default)" }
    let apiKey: String?
    let maxTokens: Int64?
    let temperature: Double?
    /// `nil` means DO NOT SEND the kwarg — models without a thinking gate
    /// reject an unknown one, so this is three-valued on purpose.
    let think: Bool?

    func chat(messages: [JSON]) -> AsyncThrowingStream<AIChunk, Error> {
        let endpoint = endpoint
        let requestedModel = requestedModel
        let apiKey = apiKey
        let maxTokens = maxTokens
        let temperature = temperature
        let think = think
        return AsyncThrowingStream { continuation in
            let task = Task {
                var body: OrderedDictionary<String, JSON> = [:]
                body["messages"] = .array(messages)
                body["stream"] = .bool(true)
                if let requestedModel { body["model"] = .string(requestedModel) }
                if let temperature { body["temperature"] = .double(temperature) }
                if let maxTokens { body["max_tokens"] = .integer(maxTokens) }
                if let think {
                    body["chat_template_kwargs"] = .object([
                        "enable_thinking": .bool(think)
                    ])
                }

                let base = endpoint.hasSuffix("/") ? String(endpoint.dropLast()) : endpoint
                var req = HTTPClientRequest(url: "\(base)/chat/completions")
                req.method = .POST
                req.headers.add(name: "Content-Type", value: "application/json")
                req.headers.add(name: "Accept", value: "text/event-stream")
                if let apiKey, !apiKey.isEmpty {
                    req.headers.add(name: "Authorization", value: "Bearer \(apiKey)")
                }
                req.body = .bytes(ByteBuffer(string: JSON.object(body).serialize()))

                do {
                    let response = try await HTTPClient.shared.execute(req, timeout: .seconds(600))
                    let code = Int(response.status.code)
                    if code >= 400 {
                        continuation.finish(throwing: LlamaCppError.http(code))
                        return
                    }
                    // ONE wrapper for the whole turn — the reasoning markers open
                    // and close once, not per chunk.
                    var wrap = ThinkingWrap()
                    for try await line in bytesToLines(response.body) {
                        let decoded = decodeOpenAISSELine(line, wrap: &wrap)
                        for token in decoded.tokens { continuation.yield(.token(token)) }
                        if decoded.done { break }
                    }
                    // The model spent the tail of its turn reasoning: close the
                    // span so the text a parser reads stays balanced.
                    for token in wrap.finish() { continuation.yield(.token(token)) }
                    continuation.finish()
                } catch {
                    continuation.finish(throwing: error)
                }
            }
            continuation.onTermination = { _ in task.cancel() }
        }
    }
}

enum LlamaCppError: Error, CustomStringConvertible {
    case http(Int)
    var description: String {
        switch self {
        case .http(let code): return "HTTP \(code)"
        }
    }
}
