// anthropic_backend — an LLM agent over Anthropic's Messages API. Mirrors python's
// `anthropic_backend` (canonical) and rust's `fantastic-anthropic-backend`.
//
// `POST {endpoint}` (the record's Messages URL) with `stream: true`. NO native tools:
// no `tools` array is sent; ai-core parses the `<call>` elements out of the text.
// ONE top-level `system`; consecutive turns of one role merged; `max_tokens` on
// every request; `thinking_budget` asks for extended thinking (`thinking_delta` →
// `<think>`); a stream without `message_stop` did not finish; an `error` event is
// raised with the API's own message. THE KEY is optional on the record
// (`credentials`, sent as `x-api-key` with `anthropic-version`).

import AsyncHTTPClient
import FantasticAICore
import FantasticJSON
import FantasticKernel
import Foundation
import NIOCore
import OrderedCollections

public let ANTHROPIC_HANDLER_MODULE = "anthropic_backend.tools"
let ANTHROPIC = "anthropic_backend"
/// The API version every request names.
let ANTHROPIC_VERSION = "2023-06-01"
/// Below this no thinking is asked for: the API needs `budget_tokens >= 1024`.
let THINKING_FLOOR: Int64 = 1024

/// ai-core's messages → (`system`, the user/assistant turns). Python `to_messages`.
func toMessages(_ messages: [JSON]) -> (String, [JSON]) {
    var systems: [String] = []
    var turns: [(String, String)] = []
    for m in messages {
        let role = m["role"].asString ?? ""
        let content: String
        switch m["content"] {
        case .string(let s): content = s
        case .null: content = ""
        case let v: content = pyStr(v)
        }
        switch role {
        case "system":
            if !content.isEmpty { systems.append(content) }
        case "user", "assistant":
            if let last = turns.last, last.0 == role {
                turns[turns.count - 1].1 += "\n\n" + content
            } else {
                turns.append((role, content))
            }
        default: break
        }
    }
    return (
        systems.joined(separator: "\n\n"),
        turns.map { .object(["role": .string($0.0), "content": .string($0.1)]) }
    )
}

/// The headers of every request; the key only when the record names one.
func anthropicHeaders(_ req: inout HTTPClientRequest, key: String?) {
    req.headers.add(name: "anthropic-version", value: ANTHROPIC_VERSION)
    req.headers.add(name: "Content-Type", value: "application/json")
    if let key, !key.isEmpty { req.headers.add(name: "x-api-key", value: key) }
}

/// The Messages provider. PURE RAW TEXT.
struct AnthropicProvider: AIProvider {
    let endpoint: String
    let model: String
    let key: KeyReader
    let maxTokens: Int64
    let temperature: Double?
    let thinkingBudget: Int64?
    let usage: AIUsage? = AIUsage()

    /// The request body. Python `AnthropicProvider.body`.
    func body(_ messages: [JSON], maxTokens bound: Int?) -> JSON {
        let (system, turns) = toMessages(messages)
        let callMax = bound.map(Int64.init) ?? maxTokens
        var b: OrderedDictionary<String, JSON> = [
            "model": .string(model), "max_tokens": .integer(callMax),
            "messages": .array(turns), "stream": .bool(true),
        ]
        if !system.isEmpty { b["system"] = .string(system) }
        if let temperature { b["temperature"] = .double(temperature) }
        if let budget = thinkingBudget, budget >= THINKING_FLOOR, budget < callMax {
            b["thinking"] = .object(["type": "enabled", "budget_tokens": .integer(budget)])
        }
        return .object(b)
    }

    func chat(messages: [JSON]) -> AsyncThrowingStream<AIChunk, Error> {
        chat(messages: messages, maxTokens: nil)
    }

    func chat(messages: [JSON], maxTokens bound: Int?) -> AsyncThrowingStream<AIChunk, Error> {
        let provider = self
        return AsyncThrowingStream { continuation in
            let task = Task {
                do {
                    try await provider.run(messages, bound: bound, continuation)
                    continuation.finish()
                } catch {
                    continuation.finish(throwing: error)
                }
            }
            continuation.onTermination = { _ in task.cancel() }
        }
    }

    private func run(
        _ messages: [JSON], bound: Int?, _ continuation: AsyncThrowingStream<AIChunk, Error>.Continuation
    ) async throws {
        var req = HTTPClientRequest(url: endpoint)
        req.method = .POST
        anthropicHeaders(&req, key: try await key(endpoint))
        req.body = .bytes(ByteBuffer(string: body(messages, maxTokens: bound).serialize()))
        let response: HTTPClientResponse
        do {
            response = try await HTTPClient.shared.execute(req, deadline: .distantFuture)
        } catch {
            throw PyError("RuntimeError", "\(ANTHROPIC): could not connect to \(endpoint)")
        }
        let code = Int(response.status.code)
        if code >= 400 {
            let collected = try? await response.body.collect(upTo: 1 << 20)
            let detail = String((collected.map { String(buffer: $0) } ?? "").prefix(500))
            let why = code == 401 || code == 403
                ? " — the API wants a key: name it in `credentials` ({store, ref})" : ""
            throw PyError(
                "RuntimeError", "\(ANTHROPIC): \(endpoint) answered HTTP \(code)\(why): \(detail)")
        }
        var wrap = ThinkingWrap()
        var stopped = false
        do {
            for try await line in bytesToLines(response.body) {
                guard line.hasPrefix("data:") else { continue }
                let data = String(line.dropFirst(5)).trimmingCharacters(in: .whitespacesAndNewlines)
                if data.isEmpty { continue }
                let evt: JSON
                do { evt = try JSON.parse(data) } catch { throw PyError("JSONDecodeError", "\(error)") }
                switch evt["type"].asString ?? "" {
                case "message_start":
                    let u = evt["message"]["usage"]
                    let total = ["input_tokens", "cache_creation_input_tokens", "cache_read_input_tokens"]
                        .reduce(Int64(0)) { $0 + (u[$1].asInt ?? 0) }
                    usage?.lastPromptTokens = total > 0 ? Int(total) : nil
                case "content_block_delta":
                    let delta = evt["delta"]
                    switch delta["type"].asString {
                    case "thinking_delta":
                        if let t = delta["thinking"].asString, !t.isEmpty {
                            for token in wrap.reasoning(t) { continuation.yield(.token(token)) }
                        }
                    case "text_delta":
                        if let t = delta["text"].asString, !t.isEmpty {
                            for token in wrap.content(t) { continuation.yield(.token(token)) }
                        }
                    default: break
                    }
                case "message_delta":
                    if let r = evt["delta"]["stop_reason"].asString { usage?.lastStopReason = r }
                    if let n = evt["usage"]["output_tokens"].asInt, n > 0 {
                        usage?.lastCompletionTokens = Int(n)
                    }
                case "message_stop":
                    stopped = true
                case "error":
                    let err = evt["error"]
                    throw PyError(
                        "RuntimeError",
                        "\(ANTHROPIC): \(endpoint) stream error: \(pyStr(err["type"])): \(pyStr(err["message"]))")
                default: break
                }
            }
        } catch let e as PyError {
            throw e
        } catch {
            throw PyError(
                "RuntimeError",
                "\(ANTHROPIC): \(endpoint) dropped the stream mid-answer; this turn is NOT complete")
        }
        for token in wrap.finish() { continuation.yield(.token(token)) }
        if !stopped {
            throw PyError(
                "RuntimeError",
                "\(ANTHROPIC): the stream from \(endpoint) ended without message_stop; this turn is "
                    + "NOT complete")
        }
        if bound == nil && usage?.lastStopReason == "max_tokens" {
            throw PyError(
                "RuntimeError",
                "\(ANTHROPIC): the turn was cut at max_tokens=\(maxTokens) (stop_reason=max_tokens) "
                    + "— it did not finish; raise max_tokens on the record, or lower thinking_budget")
        }
    }
}

public final class AnthropicBundle: AgentBundle, @unchecked Sendable {
    public let name = "anthropic_backend"
    public var verbs: [String] { AIBackend.verbNames }
    public var verbDocTable: JSON { .object(AIBackend.VERBS) }
    public var emitsDoc: JSON? { .object(AIBackend.REFLECT_EMITS) }
    public var cancelDoc: JSON? { .string(CANCEL_DOC) }
    public var notesDoc: JSON? { .object(AIBackend.NOTES) }

    let core: AIBackend

    public init() {
        self.core = buildAIBackend(
            AIBackendConfig(
                kind: ANTHROPIC,
                sentence: "Anthropic Messages-API LLM agent (raw prompt-and-parse tool-calling).",
                reflectExtra: { agent, _ in
                    [
                        "max_tokens": agent.metaValue(forKey: "max_tokens") ?? .null,
                        "temperature": agent.metaValue(forKey: "temperature") ?? .null,
                        "thinking_budget": agent.metaValue(forKey: "thinking_budget") ?? .null,
                    ]
                },
                makeProvider: { agent, _, kernel in
                    try Self.makeProvider(
                        agent, key: keyReader(agent.id, kernel, backend: ANTHROPIC, required: false))
                }
            ))
    }

    public var readme: String? { Self.readmeText }

    public func handle(agentId: AgentId, payload: JSON, kernel: Kernel) async throws -> JSON? {
        // A verb that runs a turn first reads what the record leaves to the API; an
        // API that does not answer refuses the verb, naming the field.
        if let verb = payload["type"].asString, AnthropicServerRead.TURN_VERBS.contains(verb),
            let refused = await AnthropicServerRead.resolve(
                agentId, kernel: kernel,
                key: keyReader(agentId, kernel, backend: ANTHROPIC, required: false),
                validate: { meta, record in self.validateMeta(meta, record: record) })
        {
            return refused
        }
        return try await core.handle(agentId: agentId, payload: payload, kernel: kernel)
    }

    public func onCancel(agentId: AgentId, taskId: TaskId, kernel: Kernel) async throws {
        try await core.onCancel(agentId: agentId, taskId: taskId, kernel: kernel)
    }

    public func onDelete(agentId: AgentId, kernel: Kernel) async throws {
        await core.onDelete(agentId: agentId, kernel: kernel)
    }

    static func requiredInt(_ agent: Agent, _ key: String) throws(PyError) -> Int64 {
        guard case .integer(let n)? = agent.metaValue(forKey: key), n >= 1 else {
            throw PyError(
                "ValueError",
                "\(ANTHROPIC): `\(key)` is not on the agent record, a positive int, and was not "
                    + "read from the API (update_agent <id> \(key)=…)")
        }
        return n
    }

    static func makeProvider(_ agent: Agent, key: @escaping KeyReader) throws(PyError) -> AnthropicProvider {
        var endpoint = try requiredMeta(agent, "endpoint", ANTHROPIC)
        while endpoint.hasSuffix("/") { endpoint.removeLast() }
        let temperature: Double?
        switch agent.metaValue(forKey: "temperature") {
        case .double(let d)?: temperature = d
        case .integer(let n)?: temperature = Double(n)
        default: temperature = nil
        }
        var budget: Int64? = nil
        if case .integer(let b)? = agent.metaValue(forKey: "thinking_budget"), b >= 1 { budget = b }
        return AnthropicProvider(
            endpoint: endpoint, model: try requiredMeta(agent, "model", ANTHROPIC), key: key,
            maxTokens: try requiredInt(agent, "max_tokens"), temperature: temperature,
            thinkingBudget: budget)
    }

    static func positiveInt(_ v: JSON?) -> Int64? {
        if case .integer(let n)? = v, n >= 1 { return n }
        return nil
    }

    /// Python `validate_meta`, text for text.
    public func validateMeta(
        _ meta: OrderedDictionary<String, JSON>, record: OrderedDictionary<String, JSON>
    ) -> String? {
        var merged = record
        for (k, v) in meta { merged[k] = v }
        for key in ["max_tokens", "context_window", "thinking_budget"] {
            if let v = meta[key], v != .null, Self.positiveInt(v) == nil {
                return "\(key) must be a positive int"
            }
        }
        if let t = meta["temperature"], t != .null {
            switch t {
            case .integer, .double: break
            default: return "temperature must be a number"
            }
        }
        if let budget = Self.positiveInt(merged["thinking_budget"]) {
            if budget < THINKING_FLOOR {
                return "thinking_budget=\(budget) is below the API's \(THINKING_FLOOR)"
            }
            if let mx = Self.positiveInt(merged["max_tokens"]), budget >= mx {
                return "thinking_budget=\(budget) is not below max_tokens=\(mx) — the API refuses "
                    + "it; keep it below max_tokens"
            }
        }
        let endpoint = pyStrip(merged["endpoint"]?.asString ?? "")
        let left = AnthropicServerRead.missing(merged)
        if !endpoint.isEmpty, let first = left.first, AnthropicServerRead.modelsURL(endpoint) == nil {
            return "\(first) is left to the API, but the endpoint '\(endpoint)' does not end in "
                + "/messages, so the model list is not known — set \(left.joined(separator: ", "))"
        }
        return RecordSchema.validate(Self.RECORD_SCHEMA, backend: ANTHROPIC, meta: meta, record: record)
    }

    public var recordSchema: JSON? { Self.RECORD_SCHEMA }

    static let FROM_API = "; left out, read from the API before a turn and written here"
    /// THE RECORD SCHEMA — python `RECORD_SCHEMA`, field for field.
    static let RECORD_SCHEMA: JSON = RecordSchema.schema([
        RecordSchema.field(
            "endpoint", "str", "the Messages API URL, e.g. https://api.anthropic.com/v1/messages",
            required: true),
        RecordSchema.field("model", "str", "the model id, e.g. claude-sonnet-5", required: true),
        RecordSchema.field(
            "max_tokens", "int", "a turn's output budget (tokens)" + FROM_API + " (the model's max_tokens)"),
        RecordSchema.field(
            "context_window", "int", "the model's input window (tokens)" + FROM_API + " (max_input_tokens)"),
        RecordSchema.field("temperature", "float", "sampling temperature; sent only when set"),
        RecordSchema.field(
            "thinking_budget", "int",
            "extended thinking per turn (tokens, at least \(THINKING_FLOOR), below max_tokens); unset, "
                + "no thinking is asked for"),
        // the API key — optional on the record: absent, no key header goes out
        RecordSchema.credentialsField(required: false),
    ])

    /// THE PYTHON FILE, BYTE FOR BYTE (`anthropic_backend/readme.md`).
    static let readmeText = #"""
    # anthropic_backend — an LLM agent over Anthropic's Messages API

    Talks to Anthropic's Messages API (`POST {endpoint}`, streamed). Per-client chat
    threads, FIFO lock, and automatic context management (compaction / recall /
    context_status). Conversation in its memory store (`memory_store_id`). **No agentic
    loop** — one provider pass per `send`. Tool-calling is this system's own `<call>`
    text: no `tools` array is sent; ai_core parses the calls out of the answer.

    ## The record

    REQUIRED — refused at the write when missing, every missing field named (the whole
    field list is this kind's `schema` in `reflect bundles=all`; `memory_store_id` is
    required too, as on every AI agent):
    - `endpoint` — the Messages URL, e.g. `https://api.anthropic.com/v1/messages`
    - `model` — the model id, e.g. `claude-sonnet-5`

    READ FROM THE API when left out — before a verb that runs a turn (`send`, `inquiry`,
    `context_recall`) each one still absent is read from the model's entry,
    `GET {endpoint with /messages replaced by /models}/{model}`, and WRITTEN BACK to the
    record (reflect shows it; cleared with `null`, it is read again):
    - `max_tokens` — a turn's output budget; left out, the entry's `max_tokens`
    - `context_window` — the input window; left out, the entry's `max_input_tokens`

    An API that does not answer refuses the verb (`bad_arg`, `arg` = the field), naming
    the URL and what came back.

    Optional: `temperature` (sent only when set), `thinking_budget` (extended thinking
    per turn — at least 1024 tokens and below `max_tokens`, refused at the write
    otherwise; unset, no thinking is asked for; a side call too small for it gets none).

    ## The key

    `credentials: {store, ref}` names the API key in a `credential_store` — read before
    each request and sent as `x-api-key` (with `anthropic-version: 2023-06-01`). No
    `credentials`, no key header: the API answers 401 and the error says to name one.

    ## The stream

    `system` is one top-level string (every system message joined); `messages` holds the
    user / assistant turns, consecutive turns of one role merged. With `thinking_budget`
    the thought streams as `thinking_delta`s, wrapped once in `<think>…</think>` (stripped
    before the turn is stored; only the answer's calls run). The stream ends with
    `message_stop`; one that ends without it did not finish. An `error` event is raised
    with the API's own type and message.

    ## Failures, in words

    - HTTP 401/403 → the API wants a key: name it in `credentials`
    - HTTP 4xx/5xx → the API's own error body, up to 500 chars
    - `stop_reason: max_tokens` on a main turn → a cut: raise `max_tokens` or lower
      `thinking_budget`; a bounded side call (a fold summary) reads it as its answer
    - the stream drops, or ends without `message_stop` → the turn is not complete

    """#
}
