// NVIDIA NIM (OpenAI-compatible) LLM backend.
//
// Mirrors Rust's `fantastic-nvidia-nim-backend::NvidiaNimBundle`.
// Talks to NIM via HTTPS POST + Bearer auth; streams tokens via
// Server-Sent Events; aggregates tool-call deltas across SSE chunks.
//
// All the agent machinery (history, epoch cancellation, verb
// dispatch, token/done events) lives in the shared
// `FantasticAICore.AIBackend`. This file keeps only NIM-specific
// wire: the Bearer/SSE/429 provider + the api_key refusal + the
// tool-call aggregation that the shared core persists into the
// assistant turn (config `persistToolCalls`).

import AsyncHTTPClient
import FantasticAICore
import FantasticJSON
import FantasticKernel
import Foundation
import NIOCore
import OrderedCollections

public let HANDLER_MODULE = "nvidia_nim_backend.tools"

public final class NvidiaNimBundle: AgentBundle, @unchecked Sendable {
    public let name = "nvidia_nim_backend"

    private let core: AIBackend

    public init() {
        self.core = buildAIBackend(
            AIBackendConfig(
                kind: "nvidia_nim_backend",
                provider: "nvidia_nim",
                sentence:
                    "NVIDIA NIM-backed LLM agent (OpenAI-compatible, native tool-calling).",
                verbs: [
                    "send": "args: text, client_id?. Streams a response via SSE.",
                    "history": "args: client_id?. Returns prior turns.",
                    "interrupt": "args: client_id?. Cancels in-flight stream.",
                    "status":
                        "args: client_id?. In-flight phase + this client's pending queue (others' text redacted).",
                    "backend_state": "Reports availability + in-flight.",
                ] as JSON,
                // NIM persists finalized tool-calls into the assistant
                // turn, and its `done` error path keeps `accumulated`.
                persistToolCalls: true,
                includeAccumulatedOnError: true,
                // OpenAI shape: tool-call arguments are a JSON string;
                // dispatch the batch serially (matches the Rust NIM port).
                toolArgsAsJson: true,
                parallelTools: false,
                reflectExtra: { agent in
                    [
                        "host": .string(Self.host(agent: agent)),
                        "model": .string(Self.model(agent: agent)),
                    ]
                },
                backendStateExtra: { agent in
                    [
                        "host": .string(Self.host(agent: agent)),
                        "model": .string(Self.model(agent: agent)),
                        "configured":
                            .bool(agent.metaValue(forKey: "api_key")?.asString != nil),
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
        Verbs: send, history, interrupt, backend_state; api_key stored in agent meta (set via update_agent); 429 rate-limit retry.
        """
    }

    public func handle(
        agentId: AgentId,
        payload: JSON,
        kernel: Kernel
    ) async throws -> JSON? {
        await core.handle(agentId: agentId, payload: payload, kernel: kernel)
    }

    // MARK: - meta helpers

    fileprivate static func host(agent: Agent) -> String {
        agent.metaValue(forKey: "host")?.asString ?? "https://integrate.api.nvidia.com"
    }

    fileprivate static func model(agent: Agent) -> String {
        agent.metaValue(forKey: "model")?.asString ?? "meta/llama-3.1-70b-instruct"
    }
}

/// NIM `/v1/chat/completions` SSE provider — Bearer auth, 429 retry
/// with exponential backoff (1s, 2s, 4s), per-index tool-call delta
/// aggregation finalized into `.toolCall` chunks at stream end.
struct NvidiaNimProvider: AIProvider {
    let host: String
    let model: String
    let apiKey: String

    func chat(messages: [JSON], tools: [JSON]) -> AsyncThrowingStream<AIChunk, Error> {
        let host = host
        let model = model
        let apiKey = apiKey
        return AsyncThrowingStream { continuation in
            let task = Task {
                var req = HTTPClientRequest(url: "\(host)/v1/chat/completions")
                req.method = .POST
                req.headers.add(name: "Content-Type", value: "application/json")
                req.headers.add(name: "Authorization", value: "Bearer \(apiKey)")
                req.headers.add(name: "Accept", value: "text/event-stream")

                var body: OrderedDictionary<String, JSON> = [:]
                body["model"] = .string(model)
                body["messages"] = .array(messages)
                body["stream"] = .bool(true)
                if !tools.isEmpty {
                    // OpenAI tool shape.
                    let wrapped = tools.map { t -> JSON in
                        .object([
                            "type": .string("function"),
                            "function": .object([
                                "name": t["name"],
                                "description": t["description"],
                                "parameters": t["parameters"],
                            ]),
                        ])
                    }
                    body["tools"] = .array(wrapped)
                }
                req.body = .bytes(ByteBuffer(string: JSON.object(body).serialize()))

                var attempt = 0
                let maxAttempts = 3
                var toolCallsAccum: OrderedDictionary<Int, OrderedDictionary<String, JSON>> = [:]

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
                        for try await line in bytesToLines(response.body) {
                            guard line.hasPrefix("data: ") else { continue }
                            let payload = String(line.dropFirst(6))
                            if payload == "[DONE]" { break }
                            guard let data = payload.data(using: .utf8),
                                let parsed = try? JSON.parse(data)
                            else { continue }
                            let choice = parsed["choices"][0]
                            let delta = choice["delta"]
                            if let chunk = delta["content"].asString {
                                continuation.yield(.token(chunk))
                            }
                            if let calls = delta["tool_calls"].asArray {
                                for call in calls {
                                    guard let idx = call["index"].asInt else { continue }
                                    var existing = toolCallsAccum[Int(idx)] ?? [:]
                                    if let id = call["id"].asString {
                                        existing["id"] = .string(id)
                                    }
                                    if let fn = call["function"].asObject {
                                        var fnExisting = existing["function"]?.asObject ?? [:]
                                        if let nm = fn["name"]?.asString {
                                            fnExisting["name"] = .string(nm)
                                        }
                                        if let args = fn["arguments"]?.asString {
                                            let prev = fnExisting["arguments"]?.asString ?? ""
                                            fnExisting["arguments"] = .string(prev + args)
                                        }
                                        existing["function"] = .object(fnExisting)
                                    }
                                    toolCallsAccum[Int(idx)] = existing
                                }
                            }
                        }
                        break  // success — exit retry loop
                    } catch {
                        continuation.finish(throwing: error)
                        return
                    }
                }

                // Finalize aggregated tool-calls (in index order; the
                // shared core re-sorts by id for the persisted turn).
                for (_, call) in toolCallsAccum {
                    continuation.yield(.toolCall(.object(call)))
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
    var description: String {
        switch self {
        case .http(let code): return "HTTP \(code)"
        }
    }
}
