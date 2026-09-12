// NVIDIA NIM (OpenAI-compatible) LLM backend.
//
// Mirrors Rust's `fantastic-nvidia-nim-backend::NvidiaNimBundle`.
// Talks to NIM via HTTPS POST + Bearer auth; streams RAW TEXT tokens via
// Server-Sent Events. NO native tool-calling — ai-core parses the
// `<call name=…>` envelope from the streamed text.
//
// All the agent machinery (history, epoch cancellation, verb
// dispatch, token/done events, raw tool-call parsing) lives in the shared
// `FantasticAICore.AIBackend`. This file keeps only NIM-specific
// wire: the Bearer/SSE/429 provider + the api_key refusal.

import AsyncHTTPClient
import FantasticAICore
import FantasticJSON
import FantasticKernel
import Foundation
import NIOCore
import OrderedCollections

public let HANDLER_MODULE = "nvidia_nim_backend.tools"

/// NO guessy default (by default, no defaults): never name a network-reaching
/// model. The sentinel exists ONLY so `reflect` can say what is true; `chat()`
/// fails loud when the model is unset or still this value.
/// (py-canonical `nvidia_nim_backend.provider.DEFAULT_MODEL`.)
public let DEFAULT_MODEL = "(no default — set the `model` meta)"

public final class NvidiaNimBundle: AgentBundle, @unchecked Sendable {
    public let name = "nvidia_nim_backend"

    private let core: AIBackend

    public init() {
        self.core = buildAIBackend(
            AIBackendConfig(
                kind: "nvidia_nim_backend",
                provider: "nvidia_nim",
                sentence:
                    "NVIDIA NIM-backed LLM agent (OpenAI-compatible; raw prompt-and-parse tool-calling).",
                // NIM's `done` error path keeps `accumulated`.
                includeAccumulatedOnError: true,
                reflectExtra: { agent in
                    [
                        "host": .string(Self.host(agent: agent)),
                        "model": .string(Self.model(agent: agent)),
                    ]
                },
                makeProvider: { agent, _, _ in
                    guard let apiKey = agent.metaValue(forKey: "api_key")?.asString,
                        !apiKey.isEmpty
                    else {
                        return .refused(
                            .object([
                                "error": .string("nvidia_nim: api_key not configured"),
                                "reason": .string("no_api_key"),
                            ]))
                    }
                    return .provider(
                        NvidiaNimProvider(
                            host: Self.host(agent: agent),
                            model: Self.model(agent: agent),
                            apiKey: apiKey))
                }
            ))
    }

    public var readme: String? {
        """
        nvidia_nim_backend — NVIDIA NIM LLM agent (OpenAI-compatible); thin over FantasticAICore.
        Verbs: the shared ai_core set (reflect it); api_key stored in agent meta (set via update_agent); 429 rate-limit retry.
        Tool-calling is RAW: no native OpenAI tools — provider streams text, FantasticAICore parses the <call name=…> envelope (tool_parse).
        """
    }

    public func handle(
        agentId: AgentId,
        payload: JSON,
        kernel: Kernel
    ) async throws -> JSON? {
        await core.handle(agentId: agentId, payload: payload, kernel: kernel)
    }

    /// The kernel is killing a task of this agent. If it is the in-flight
    /// generation, close the turn out for the caller before the substrate
    /// cancels it (which drops the SSE stream + releases the FIFO lock on its
    /// own). There is no `interrupt` verb any more: cancellation is the kernel's.
    public func onCancel(agentId: AgentId, taskId: TaskId, kernel: Kernel) async throws {
        await core.onCancel(agentId: agentId, taskId: taskId, kernel: kernel)
    }

    // MARK: - meta helpers

    fileprivate static func host(agent: Agent) -> String {
        agent.metaValue(forKey: "host")?.asString ?? "https://integrate.api.nvidia.com"
    }

    fileprivate static func model(agent: Agent) -> String {
        agent.metaValue(forKey: "model")?.asString ?? DEFAULT_MODEL
    }
}

/// NIM `/v1/chat/completions` SSE provider — Bearer auth, 429 retry with
/// exponential backoff (1s, 2s, 4s). PURE RAW TEXT: yields `.token`s only,
/// decoded from BOTH of the model's channels (`reasoning_content` wrapped in
/// `<think>…</think>`, `content` bare — see `decodeNimSSELine`); no native
/// tool-calls (ai-core parses the `<call name=…>` envelope from the text).
struct NvidiaNimProvider: AIProvider {
    let host: String
    let model: String
    let apiKey: String

    func chat(messages: [JSON]) -> AsyncThrowingStream<AIChunk, Error> {
        let host = host
        let model = model
        let apiKey = apiKey
        return AsyncThrowingStream { continuation in
            let task = Task {
                if model.isEmpty || model == DEFAULT_MODEL {
                    continuation.finish(
                        throwing: NimError.noModel)
                    return
                }
                var req = HTTPClientRequest(url: "\(host)/v1/chat/completions")
                req.method = .POST
                req.headers.add(name: "Content-Type", value: "application/json")
                req.headers.add(name: "Authorization", value: "Bearer \(apiKey)")
                req.headers.add(name: "Accept", value: "text/event-stream")

                // RAW: no native `tools` array — ai-core parses the `<call name=…>`
                // envelope out of the streamed content text.
                var body: OrderedDictionary<String, JSON> = [:]
                body["model"] = .string(model)
                body["messages"] = .array(messages)
                body["stream"] = .bool(true)
                req.body = .bytes(ByteBuffer(string: JSON.object(body).serialize()))

                var attempt = 0
                let maxAttempts = 3

                while attempt < maxAttempts {
                    attempt += 1
                    do {
                        let response = try await HTTPClient.shared.execute(
                            req, timeout: .seconds(600))
                        let statusCode = Int(response.status.code)
                        if statusCode == 429 {
                            let backoffMs = UInt64(1000 * (1 << (attempt - 1)))
                            try? await Task.sleep(nanoseconds: backoffMs * 1_000_000)
                            continue
                        }
                        if statusCode >= 400 {
                            continuation.finish(throwing: NimError.http(statusCode))
                            return
                        }
                        // ONE wrapper for the whole turn — the reasoning markers
                        // open and close once, not per chunk (`ThinkingWrap`).
                        // Built here, after the 429 retry decision: a retried
                        // attempt streamed nothing, so it starts clean.
                        var wrap = ThinkingWrap()
                        for try await line in bytesToLines(response.body) {
                            let decoded = decodeNimSSELine(line, wrap: &wrap)
                            for token in decoded.tokens { continuation.yield(.token(token)) }
                            if decoded.done { break }
                        }
                        // The model spent the tail of its turn (or all of it)
                        // reasoning: close the span so the text a reader parses
                        // stays balanced.
                        for token in wrap.finish() { continuation.yield(.token(token)) }
                        break  // success — exit retry loop
                    } catch {
                        continuation.finish(throwing: error)
                        return
                    }
                }
                continuation.finish()
            }
            continuation.onTermination = { _ in task.cancel() }
        }
    }
}

/// HTTP failure that carries the status code, formatted to match the
/// prior inline `"HTTP \(code)"` done-event error string exactly.
private enum NimError: Error, CustomStringConvertible {
    case http(Int)
    case noModel
    var description: String {
        switch self {
        case .http(let code): return "HTTP \(code)"
        case .noModel:
            return "nvidia_nim_backend: no `model` set — this connector does not "
                + "guess one. Name a model on the agent record (model=...)."
        }
    }
}
