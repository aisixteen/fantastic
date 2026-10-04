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

/// Sampling temperature when the record names none — LOW, because an agentic
/// backend must emit exact calls, not improvise. Python `DEFAULT_TEMPERATURE`.
public let DEFAULT_TEMPERATURE = 0.3

public final class OllamaBackendBundle: AgentBundle, @unchecked Sendable {
    public let name = "ollama_backend"
    public var verbs: [String] { AIBackend.verbNames }

    let core: AIBackend

    public init() {
        self.core = buildAIBackend(
            AIBackendConfig(
                kind: "ollama_backend",
                sentence: "Ollama-backed LLM agent (raw prompt-and-parse tool-calling).",
                // The SAMPLING KNOBS, in python's order — `model` and `endpoint`
                // are base fields now. `temperature` falls back to the provider's
                // 0.3; the rest are null unless the record sets them.
                reflectExtra: { agent, _ in
                    [
                        "temperature": .double(Self.temperature(agent: agent)),
                        "num_ctx": Self.numCtx(agent: agent).map(JSON.integer) ?? .null,
                        "num_predict": Self.numPredict(agent: agent).map(JSON.integer) ?? .null,
                        "think": Self.think(agent: agent).map(JSON.bool) ?? .null,
                    ]
                },
                makeProvider: { agent, _, _ in
                    // `endpoint` and `model` are REQUIRED on the record — nothing
                    // is guessed; an absent one fails the turn loud, naming it.
                    OllamaProvider(
                        endpoint: try requiredMeta(agent, "endpoint", "ollama_backend"),
                        model: try requiredMeta(agent, "model", "ollama_backend"),
                        temperature: Self.temperature(agent: agent),
                        numCtx: Self.numCtx(agent: agent),
                        numPredict: Self.numPredict(agent: agent),
                        think: Self.think(agent: agent))
                }
            ))
    }

    public var readme: String? { Self.readmeText }

    /// Refuse, AT THE WRITE, a record missing a required field or naming an enum
    /// value outside its set — every missing field named at once, from the schema.
    /// Python `validate_meta`.
    public func validateMeta(
        _ meta: OrderedDictionary<String, JSON>, record: OrderedDictionary<String, JSON>
    ) -> String? {
        RecordSchema.validate(Self.RECORD_SCHEMA, backend: name, meta: meta, record: record)
    }

    public var recordSchema: JSON? { Self.RECORD_SCHEMA }

    /// THE RECORD SCHEMA (lib request #3) — what this kind's record takes, read with
    /// no agent existing (`reflect bundles=all`), from the constants the provider
    /// runs on. `validateMeta` enforces it at the write, so the page and the kernel
    /// cannot disagree about what a record needs. Python `RECORD_SCHEMA`.
    static let RECORD_SCHEMA: JSON = RecordSchema.schema(
        [
            RecordSchema.field("endpoint", "str", "the ollama server URL", required: true),
            RecordSchema.field("model", "str", "the model name", required: true),
            RecordSchema.field(
                "num_ctx", "int", "the context window ollama loads the model with"),
            RecordSchema.field(
                "temperature", "float", "sampling temperature",
                default: .double(DEFAULT_TEMPERATURE)),
            RecordSchema.field("num_predict", "int", "a turn's output budget (tokens)"),
            RecordSchema.field("think", "bool", "turn the model's thinking channel on or off"),
        ])

    /// THE PYTHON FILE, BYTE FOR BYTE (`ollama_backend/readme.md`). Persisted as
    /// `agents/<id>/readme.md` and served by `reflect readme=true`, so a swift
    /// kernel and a python one hand a reader the same document. A swift-written
    /// summary of it was a second, shorter truth about the same bundle.
    static let readmeText = #"""
    # ollama_backend — local LLM, a context-managed completion agent
    Talks to a local ollama. Per-client chat threads, FIFO lock, and automatic
    context management (compaction / recall / context_status). Conversation in its
    memory store (`memory_store_id`). **No agentic loop** — one provider pass per `send`.

    ## Implementation
    A thin binding over the shared `ai_core` lib. The queue / FIFO lock, prompt
    assembly, the ONE-PASS `_run`, and all the context machinery live in
    `ai_core.core`; this module supplies the `OllamaProvider` builder (endpoint /
    model from the agent record) and `ai_core.build()` wires the rest into
    `(VERBS, handler)`.

    ## Bare completion + the `<call>` grammar — no native tool API
    `send` performs exactly ONE provider pass: the `OllamaProvider` streams the model's text and never receives a `tools` array nor reads `message.tool_calls`. Tool-calling is RAW and shared by every backend: `ai_core` parses the model's `<call name=…>` tags from the streamed text, dispatches them as ONE batch (`kernel.send([...])`), and each destination's results arrive as ONE fresh turn — there is no loop inside a pass.

    ## Context management — owned HERE
    The conversation lives in this agent's MEMORY STORE — `memory_store_id` on the record
    is REQUIRED (a `send` fails loud naming it): every turn, fold and view is a row under
    this agent's key. Reply BODIES are never stored — a reply row keeps its one-line
    `origin`, and after a restart the model re-reads from it. Compaction fires at 0.8 of
    the budget (calibrated by the endpoint's reported prompt size when it reports one)
    with the per-agent `context_strategy` (default `compact`; `fold`, `truncate`) and a
    `[context-notice]`; `context_status`, `context_recall`, `history {fold?}`, `grep` and
    `context_clear` (a boundary, nothing deleted) inspect and page it. `watch {body?}` /
    `unwatch` subscribe to this agent's row events (a digest; with `body` the content too).
    ## Calling this agent
    `send {type:'send', text, client_id?}` runs ONE completion (per-backend FIFO lock)
    and returns `{response, final}`. A first-class peer of `exec_bridge`: a
    scheduler, a host job, a peer kernel, or a routine can call it by id. Optional
    `system_prompt:str` REPLACES the default system block (the caller supplies its own
    role / tool convention / whatever menu it built).
    The record's own `system_prompt` (a non-empty string; `null` clears it) replaces
    the default block on EVERY turn of this agent — a main turn, a nudge, a turn a reply
    wakes — read live each turn; a per-call `system_prompt` beats it for that delivery.
    There is no caller-supplied history: a one-off completion is a disposable agent with its own store; context for another agent travels in the text.

    ## Recursion guard
    A `send` carrying a reserved `_call_stack` that already contains this agent (a
    cycle) or exceeds depth 8 is refused BEFORE the FIFO lock â so an authored agent's AI→AI
    chain can neither deadlock nor run away. Don't set `_call_stack` yourself.

    ## Meta-possibility — any routine orchestrates the whole substrate
    Every routine (an authored agent, a host job, a peer kernel, any out-of-process caller)
    reaches every agent by id. From any of them you can read memory anywhere
    (`send(<state>, {read})`), run an inference turn (`send(<ai>, {send, ...})`),
    and/or spawn compute (`send(<py>, {start, code})`) — regardless of which kernel
    owns the target. Code steps and LLM-call steps are interchangeable, both
    directions.

    """#

    public func handle(
        agentId: AgentId,
        payload: JSON,
        kernel: Kernel
    ) async throws -> JSON? {
        try await core.handle(agentId: agentId, payload: payload, kernel: kernel)
    }

    /// The kernel is killing a task of this agent. If it is the in-flight
    /// generation, close the turn out for the caller before the substrate
    /// cancels it (which drops the HTTP stream + releases the FIFO lock on its
    /// own). There is no `interrupt` verb any more: cancellation is the kernel's.
    public func onCancel(agentId: AgentId, taskId: TaskId, kernel: Kernel) async throws {
        try await core.onCancel(agentId: agentId, taskId: taskId, kernel: kernel)
    }

    /// The shared delete hook: the deleted id's AI-core rows go with it.
    public func onDelete(agentId: AgentId, kernel: Kernel) async throws {
        await core.onDelete(agentId: agentId, kernel: kernel)
    }

    // MARK: - meta helpers

    // Sampling knobs (parity with the rust/python ollama backends). ollama's own
    // defaults (temperature 0.8, num_ctx 4096) are wrong for an agentic backend;
    // temperature defaults LOW (0.3), the rest pass through only when set.
    fileprivate static func temperature(agent: Agent) -> Double {
        agent.metaValue(forKey: "temperature")?.asDouble ?? DEFAULT_TEMPERATURE
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
/// `<call name=…>` envelope from the text. Mirrors Rust/Python ollama providers.
struct OllamaProvider: AIProvider {
    let endpoint: String
    let model: String
    let temperature: Double
    let numCtx: Int64?
    let numPredict: Int64?
    let think: Bool?
    /// What the endpoint REPORTED for the last pass — the prompt size (ai_core
    /// calibrates its own estimate with it) and the stop reason.
    let usage: AIUsage? = AIUsage()

    func chat(messages: [JSON]) -> AsyncThrowingStream<AIChunk, Error> {
        chat(messages: messages, maxTokens: nil)
    }

    func chat(messages: [JSON], maxTokens: Int?) -> AsyncThrowingStream<AIChunk, Error> {
        let endpoint = endpoint
        let model = model
        let temperature = temperature
        let numCtx = numCtx
        let numPredict = numPredict
        let think = think
        let usage = usage
        return AsyncThrowingStream { continuation in
            let task = Task {
                // FAILS LOUD, INSIDE THE TURN — python's wording, verbatim.
                if model.isEmpty {
                    continuation.finish(
                        throwing: PyError(
                            "RuntimeError",
                            "ollama_backend: no `model` set — this connector does not guess one. "
                                + "Name a model on the agent record (model=...)."))
                    return
                }
                // RAW: no native `tools` array — ai-core parses the `<call name=…>`
                // envelope out of the streamed content text.
                var options: OrderedDictionary<String, JSON> = ["temperature": .double(temperature)]
                if let n = numCtx { options["num_ctx"] = .integer(n) }
                if let maxTokens {
                    // a side call's own bound (a fold summary's budget)
                    options["num_predict"] = .integer(Int64(maxTokens))
                } else if let n = numPredict {
                    options["num_predict"] = .integer(n)
                }
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
                var req = HTTPClientRequest(url: "\(endpoint)/api/chat")
                req.method = .POST
                req.headers.add(name: "Content-Type", value: "application/json")
                req.body = .bytes(ByteBuffer(string: body.serialize()))
                do {
                    let response = try await HTTPClient.shared.execute(req, timeout: .seconds(600))
                    // RAISE ON A NON-2xx, exactly where python's ollama client does.
                    // Its `_request` streams under `r.raise_for_status()` and turns
                    // an `HTTPStatusError` into `ResponseError(r.text, r.status_code)`,
                    // whose `__str__` is `<error> (status code: <n>)` and whose
                    // `error` is the body's JSON `error` field when the body parses,
                    // else the raw text. Swift read no status at all: a 500 whose
                    // body is `{"error": …}` decoded as an unparseable NDJSON line,
                    // was SKIPPED, and the turn settled `completed` with an empty
                    // response — the harness then saw a second, successful request
                    // where python raised on the first.
                    if response.status.code >= 400 {
                        let buf = try? await response.body.collect(upTo: 1 << 20)
                        let raw = buf.map { String(buffer: $0) } ?? ""
                        let parsed = try? JSON.parse(raw)
                        let msg = parsed?["error"].asString ?? raw
                        continuation.finish(
                            throwing: PyError(
                                "ResponseError",
                                "\(msg) (status code: \(Int(response.status.code)))"))
                        return
                    }
                    // ONE wrapper for the whole turn — the reasoning markers open
                    // and close once, not per chunk (see `ThinkingWrap`).
                    var wrap = ThinkingWrap()
                    for try await line in bytesToLines(response.body) {
                        // PROVIDER-REPORTED USAGE — ai_core calibrates its estimate
                        // with it.
                        let (reported, stop) = ollamaReport(line)
                        if let reported { usage?.lastPromptTokens = reported }
                        if let stop { usage?.lastStopReason = stop }
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
