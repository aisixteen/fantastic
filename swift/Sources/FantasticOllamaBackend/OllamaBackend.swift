// Ollama LLM backend.
//
// Mirrors Rust's `fantastic-ollama-backend::OllamaBackendBundle`.
// Talks to a local ollama HTTP server (default localhost:11434),
// streams tokens via URLSession's AsyncBytes iteration. All the
// agent machinery (per-(agent, client_id) history, FIFO/epoch
// cancellation, verb dispatch, token/done events) lives in the shared
// `FantasticAICore.AIBackend`; this file is just the ollama wire
// (`AIProvider` impl) + the `buildAIBackend` config.

import FantasticAICore
import FantasticJSON
import FantasticKernel
import Foundation

public let HANDLER_MODULE = "ollama_backend.tools"

public final class OllamaBackendBundle: AgentBundle, @unchecked Sendable {
    public let name = "ollama_backend"

    private let core: AIBackend

    public init() {
        self.core = buildAIBackend(
            AIBackendConfig(
                kind: "ollama_backend",
                provider: "ollama",
                sentence: "Ollama-backed LLM agent (native tool-calling).",
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
        Per-client chat threads, FIFO lock, native tool-calls; verbs: send, history, interrupt, backend_state.
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

/// Ollama `/api/chat` streaming provider — `.token` per NDJSON chunk
/// carrying `message.content`, plus a finalized `.toolCall` per
/// `message.tool_calls` entry (arguments already a parsed object).
/// Mirrors Rust's ollama provider so the shared agentic loop drives it
/// identically.
struct OllamaProvider: AIProvider {
    let host: String
    let model: String

    func chat(messages: [JSON], tools: [JSON]) -> AsyncThrowingStream<AIChunk, Error> {
        let host = host
        let model = model
        return AsyncThrowingStream { continuation in
            let task = Task {
                let url = URL(string: "\(host)/api/chat")!
                var req = URLRequest(url: url)
                req.httpMethod = "POST"
                req.setValue("application/json", forHTTPHeaderField: "Content-Type")
                let body: JSON = .object([
                    "model": .string(model),
                    "messages": .array(messages),
                    "tools": .array(tools),
                    "stream": .bool(true),
                ])
                req.httpBody = body.serialize().data(using: .utf8)
                do {
                    let (bytes, _) = try await URLSession.shared.bytes(for: req)
                    for try await line in bytes.lines {
                        guard let data = line.data(using: .utf8),
                            let parsed = try? JSON.parse(data)
                        else { continue }
                        let msg = parsed["message"]
                        if let delta = msg["content"].asString, !delta.isEmpty {
                            continuation.yield(.token(delta))
                        }
                        // ollama's `arguments` is already a parsed object;
                        // ids are absent, so mint one. Shape the chunk as
                        // the OpenAI-style `{id, function:{name, arguments}}`
                        // the shared loop expects.
                        if let calls = msg["tool_calls"].asArray {
                            for call in calls {
                                let fn = call["function"]
                                let name = fn["name"].asString ?? "send"
                                let args = fn["arguments"]
                                let id =
                                    call["id"].asString
                                    ?? "call_\(UUID().uuidString.prefix(8))"
                                continuation.yield(
                                    .toolCall(
                                        .object([
                                            "id": .string(id),
                                            "type": .string("function"),
                                            "function": .object([
                                                "name": .string(name),
                                                "arguments": args,
                                            ]),
                                        ])))
                            }
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
