// Ollama LLM backend.
//
// Mirrors Rust's `fantastic-ollama-backend::OllamaBackendBundle`.
// Talks to a local ollama HTTP server (default localhost:11434),
// streams tokens via URLSession's AsyncBytes iteration. All the
// agent machinery (per-(agent, client_id) history, FIFO/epoch
// cancellation, verb dispatch, token/done events) lives in the shared
// `FantasticAICore.AIBackend`; this file is just the ollama wire
// (`AIProvider` impl) + the `buildAIBackend` config.

import AsyncHTTPClient
import FantasticAICore
import FantasticJSON
import FantasticKernel
import Foundation
import NIOCore

public let HANDLER_MODULE = "ollama_backend.tools"

public final class OllamaBackendBundle: AgentBundle, @unchecked Sendable {
    public let name = "ollama_backend"

    private let core: AIBackend

    public init() {
        self.core = buildAIBackend(
            AIBackendConfig(
                kind: "ollama_backend",
                provider: "ollama",
                sentence: "Ollama-backed LLM agent (raw prompt-and-parse tool-calling).",
                verbs: [
                    "send": "args: text, client_id?. Streams a response.",
                    "history": "args: client_id?. Returns prior turns.",
                    "interrupt": "args: client_id?. Cancels in-flight stream.",
                    "status":
                        "args: client_id?. In-flight phase + this client's pending queue (others' text redacted).",
                    "backend_state": "Reports availability + in-flight.",
                ] as JSON,
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
                    ]
                },
                makeProvider: { agent, _, _ in
                    .provider(
                        OllamaProvider(
                            host: Self.host(agent: agent),
                            model: Self.model(agent: agent)))
                }
            ))
    }

    public var readme: String? {
        """
        ollama_backend — local LLM agent; thin over FantasticAICore.
        Per-client chat threads, FIFO lock; verbs: send, history, interrupt, backend_state.
        Tool-calling is RAW: no native ollama tools — provider streams text, FantasticAICore parses the <tool_call>/<tool_response> envelope (tool_parse).
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
        agent.metaValue(forKey: "host")?.asString ?? "http://127.0.0.1:11434"
    }

    fileprivate static func model(agent: Agent) -> String {
        agent.metaValue(forKey: "model")?.asString ?? "llama3.2"
    }
}

/// Ollama `/api/chat` streaming provider — PURE RAW TEXT: `.token` per NDJSON
/// chunk carrying `message.content`. NO native `tools` array is sent and NO
/// `tool_calls` are read; ai-core parses the `<tool_call>` envelope from the
/// content text. Mirrors Rust/Python ollama providers.
struct OllamaProvider: AIProvider {
    let host: String
    let model: String

    func chat(messages: [JSON]) -> AsyncThrowingStream<AIChunk, Error> {
        let host = host
        let model = model
        return AsyncThrowingStream { continuation in
            let task = Task {
                // RAW: no native `tools` array — ai-core parses the `<tool_call>`
                // envelope out of the streamed content text.
                let body: JSON = .object([
                    "model": .string(model),
                    "messages": .array(messages),
                    "stream": .bool(true),
                ])
                var req = HTTPClientRequest(url: "\(host)/api/chat")
                req.method = .POST
                req.headers.add(name: "Content-Type", value: "application/json")
                req.body = .bytes(ByteBuffer(string: body.serialize()))
                do {
                    let response = try await HTTPClient.shared.execute(req, timeout: .seconds(600))
                    for try await line in bytesToLines(response.body) {
                        guard let data = line.data(using: .utf8),
                            let parsed = try? JSON.parse(data)
                        else { continue }
                        let msg = parsed["message"]
                        if let delta = msg["content"].asString, !delta.isEmpty {
                            continuation.yield(.token(delta))
                        }
                        if parsed["done"].asBool == true {
                            break
                        }
                    }
                    continuation.finish()
                } catch {
                    continuation.finish(throwing: error)
                }
            }
            continuation.onTermination = { _ in task.cancel() }
        }
    }
}
