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
import OrderedCollections
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
                    "cancel": .string(CANCEL_DOC),
                    "status":
                        "args: client_id?. In-flight phase + this client's pending queue (others' text redacted).",
                    "backend_state": "Reports availability + in-flight.",
                ] as JSON,
                reflectExtra: { agent in
                    [
                        "host": .string(Self.host(agent: agent)),
                        "model": Self.model(agent: agent).map(JSON.string) ?? .null,
                    ]
                },
                backendStateExtra: { agent in
                    [
                        "host": .string(Self.host(agent: agent)),
                        "model": Self.model(agent: agent).map(JSON.string) ?? .null,
                    ]
                },
                makeProvider: { agent, _, _ in
                    guard let model = Self.model(agent: agent) else {
                        return .refused(
                            .object([
                                "error": .string(
                                    "ollama_backend: `model` required — set it on the record "
                                        + "(e.g. model=gemma4:12b). No model default is guessed.")
                            ]))
                    }
                    return .provider(
                        OllamaProvider(
                            host: Self.host(agent: agent),
                            model: model,
                            temperature: Self.temperature(agent: agent),
                            numCtx: Self.numCtx(agent: agent),
                            numPredict: Self.numPredict(agent: agent),
                            think: Self.think(agent: agent)))
                }
            ))
    }

    public var readme: String? {
        """
        ollama_backend — local LLM agent; thin over FantasticAICore.
        Per-client chat threads, FIFO lock; verbs: send, history, status, backend_state. \
        No `interrupt`: an in-flight `send` IS a kernel task — kill it with `ps` + `cancel`.
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

    /// The kernel is killing a task of this agent. If it is the in-flight
    /// generation, close the turn out for the caller before the substrate
    /// cancels it (which drops the HTTP stream + releases the FIFO lock on its
    /// own). There is no `interrupt` verb any more: cancellation is the kernel's.
    public func onCancel(agentId: AgentId, taskId: TaskId, kernel: Kernel) async throws {
        await core.onCancel(agentId: agentId, taskId: taskId, kernel: kernel)
    }

    // MARK: - meta helpers

    fileprivate static func host(agent: Agent) -> String {
        agent.metaValue(forKey: "host")?.asString ?? "http://127.0.0.1:11434"
    }

    /// The model id — REQUIRED (from the record's `model` meta). No default is
    /// guessed: a bad guess reaches the network (a wrong model 404s). `nil` when
    /// unset → the backend refuses loudly (see `makeProvider`).
    fileprivate static func model(agent: Agent) -> String? {
        agent.metaValue(forKey: "model")?.asString
    }

    // Sampling knobs (parity with the rust/python ollama backends). ollama's own
    // defaults (temperature 0.8, num_ctx 4096) are wrong for an agentic backend;
    // temperature defaults LOW (0.3), the rest pass through only when set.
    fileprivate static func temperature(agent: Agent) -> Double {
        agent.metaValue(forKey: "temperature")?.asDouble ?? 0.3
    }
    fileprivate static func numCtx(agent: Agent) -> Int64? {
        agent.metaValue(forKey: "num_ctx")?.asInt
    }
    fileprivate static func numPredict(agent: Agent) -> Int64? {
        agent.metaValue(forKey: "num_predict")?.asInt
    }
    fileprivate static func think(agent: Agent) -> Bool? {
        agent.metaValue(forKey: "think")?.asBool
    }
}

/// Ollama `/api/chat` streaming provider — PURE RAW TEXT: `.token`s decoded per
/// NDJSON line from BOTH of the model's channels (`message.thinking` wrapped in
/// `<think>…</think>`, `message.content` bare — see `decodeOllamaLine`). NO
/// native `tools` array is sent and NO `tool_calls` are read; ai-core parses the
/// `<tool_call>` envelope from the text. Mirrors Rust/Python ollama providers.
struct OllamaProvider: AIProvider {
    let host: String
    let model: String
    let temperature: Double
    let numCtx: Int64?
    let numPredict: Int64?
    let think: Bool?

    func chat(messages: [JSON]) -> AsyncThrowingStream<AIChunk, Error> {
        let host = host
        let model = model
        let temperature = temperature
        let numCtx = numCtx
        let numPredict = numPredict
        let think = think
        return AsyncThrowingStream { continuation in
            let task = Task {
                // RAW: no native `tools` array — ai-core parses the `<tool_call>`
                // envelope out of the streamed content text.
                var options: OrderedDictionary<String, JSON> = ["temperature": .double(temperature)]
                if let n = numCtx { options["num_ctx"] = .integer(n) }
                if let n = numPredict { options["num_predict"] = .integer(n) }
                var fields: OrderedDictionary<String, JSON> = [
                    "model": .string(model),
                    "messages": .array(messages),
                    "stream": .bool(true),
                    "options": .object(options),
                ]
                // `think` is a TOP-LEVEL field; only sent when set (non-thinking
                // models reject it).
                if let t = think { fields["think"] = .bool(t) }
                let body: JSON = .object(fields)
                var req = HTTPClientRequest(url: "\(host)/api/chat")
                req.method = .POST
                req.headers.add(name: "Content-Type", value: "application/json")
                req.body = .bytes(ByteBuffer(string: body.serialize()))
                do {
                    let response = try await HTTPClient.shared.execute(req, timeout: .seconds(600))
                    // ONE wrapper for the whole turn — the reasoning markers open
                    // and close once, not per chunk (see `ThinkingWrap`).
                    var wrap = ThinkingWrap()
                    for try await line in bytesToLines(response.body) {
                        let decoded = decodeOllamaLine(line, wrap: &wrap)
                        for token in decoded.tokens { continuation.yield(.token(token)) }
                        if decoded.done { break }
                    }
                    // The model spent the tail of its turn (or all of it) thinking:
                    // close the span so the text a reader parses stays balanced.
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
