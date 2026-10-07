// OpenAI Responses-API LLM backend.
//
// Mirrors python's `responses_backend` (canonical) and Rust's
// `fantastic-responses-backend`. POSTs the full `…/v1/responses` URL named on the
// record with `Authorization: Bearer <key>` — the key read before each request from
// the record's `credentials` ref in a credential store (`Credentials.swift`); streams
// RAW TEXT through the Responses event-typed Server-Sent Events.
//
// STATELESS: `store:false` and the whole conversation as typed `input` items every
// pass — the conversation is the agent's, never the endpoint's. NO NATIVE TOOLS: a
// `function_call` output item is a loud error (the SSE decoder). All the agent
// machinery lives in the shared `FantasticAICore.AIBackend`; this file keeps only
// the Responses wire.

import AsyncHTTPClient
import FantasticAICore
import FantasticJSON
import FantasticKernel
import Foundation
import NIOCore
import OrderedCollections

public let HANDLER_MODULE = "responses_backend.tools"

/// Per-turn output budget (`max_output_tokens`) when the record names none.
public let DEFAULT_MAX_TOKENS = 8192

public final class ResponsesBundle: AgentBundle, @unchecked Sendable {
    public let name = "responses_backend"
    public var verbs: [String] { AIBackend.verbNames }
    public var verbDocTable: JSON { .object(AIBackend.VERBS) }
    public var emitsDoc: JSON? { .object(AIBackend.REFLECT_EMITS) }
    public var cancelDoc: JSON? { .string(CANCEL_DOC) }
    public var notesDoc: JSON? { .object(AIBackend.NOTES) }

    let core: AIBackend

    public init() {
        self.core = buildAIBackend(
            AIBackendConfig(
                kind: "responses_backend",
                sentence:
                    "OpenAI Responses-API LLM agent (raw prompt-and-parse tool-calling).",
                // NO reflect extras: python's binding passes no `reflect_extra`.
                makeProvider: { agent, _, kernel in
                    // `endpoint` and `model` are REQUIRED on the record; the key
                    // is read INSIDE `chat()`, from the record's `credentials`,
                    // so an unwired connector reaches `status(thinking)` and then
                    // raises — python's order.
                    try ResponsesProvider(
                        endpoint: try requiredMeta(agent, "endpoint", "responses_backend"),
                        model: try requiredMeta(agent, "model", "responses_backend"),
                        key: keyReader(agent.id, kernel, backend: "responses_backend", required: true),
                        maxTokens: Self.maxTokens(agent.metaValue(forKey: "max_tokens")),
                        sampling: Self.sampling(agent.metaValue(forKey: "sampling")))
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
            RecordSchema.field(
                "endpoint", "str", "the full Responses API URL (…/v1/responses)", required: true),
            RecordSchema.field("model", "str", "the model name", required: true),
            RecordSchema.field(
                "max_tokens", "int", "a turn's output budget (tokens)",
                default: .integer(Int64(DEFAULT_MAX_TOKENS))),
            RecordSchema.field(
                "sampling", "map", "runner params merged verbatim into the request body"),
            RecordSchema.credentialsField(required: true),
        ])

    /// THE PYTHON FILE, BYTE FOR BYTE (`responses_backend/readme.md`).
    static let readmeText = #"""
    # responses_backend — an OpenAI Responses-API, context-managed completion agent
    Talks to any endpoint that serves the OpenAI Responses API (`POST …/v1/responses`): OpenAI itself, Ollama ≥ 0.13.4, or a proxy in front of an open-weights model. `endpoint` (the full `…/v1/responses` URL) and `model` are REQUIRED on the record — nothing is guessed; `credentials` is REQUIRED too: the key is read before each request from the record's `credentials: {store, ref}` — a ref in a `credential_store`, sent only to the ref's host — never from the environment or the record. Per-client chat threads, FIFO lock, and automatic context management. **No agentic loop** — one provider pass per `send`. Conversation in its memory store (`memory_store_id`).

    ## Implementation
    This bundle is a thin binding over the shared `ai_core` lib. The queue / FIFO lock, prompt assembly, the ONE-PASS `_run`, and all the context machinery + verb bodies live in `ai_core.core`; this module supplies the `ResponsesProvider` builder (endpoint/model from the agent record) and `ai_core.build()` wires the rest into `(VERBS, handler)`.

    ## The wire
    Stateless: every pass sends `store:false` and the whole conversation as `input` items (the system block as `instructions`) — the conversation is this agent's, never the endpoint's. The stream is read on BOTH channels: `response.reasoning_text` / `reasoning_summary_text` deltas are wrapped in `<think>…</think>` (stripped before the turn is stored; only the answer's calls run), `response.output_text` deltas are the output. A turn ends `completed`; one cut at `max_output_tokens` (`response.incomplete`) is a loud error on a main turn and an answer (`last_stop_reason:"max_tokens"`) on a bounded side call such as a fold summary. Sampling knobs on the record (`sampling`) merge verbatim into the request body.

    ## Bare completion + the `<call>` grammar — no native tool API
    `send` performs exactly ONE provider pass: the stream's text is read; it never sends Responses `tools` nor reads `function_call` items (one arriving is a loud error). Tool-calling is RAW and shared by every backend: `ai_core` parses the model's `<call name=…>` tags from the streamed text, dispatches them as ONE batch (`kernel.send([...])`), and each destination's results arrive as ONE fresh turn — there is no loop inside a pass.

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
    and returns `{response, final}`. A first-class peer of `exec_bridge`: a scheduler,
    a host job, a peer kernel, or a routine can call it by id. Optional `system_prompt:str`
    REPLACES the default system block (the caller supplies its own role / tool convention).
    The record's own `system_prompt` (a non-empty string; `null` clears it) replaces
    the default block on EVERY turn of this agent — a main turn, a nudge, a turn a reply
    wakes — read live each turn; a per-call `system_prompt` beats it for that delivery.
    There is no caller-supplied history: a one-off completion is a disposable agent with its own store; context for another agent travels in the text.

    ## Streaming
    You stream `token`/`status`/`done` on your OWN id. Any caller that `watch`es your id
    consumes the same stream (the driving caller, a browser tab, a peer). You do not route
    your own output — you complete; the caller reads it and decides what to do next.

    ## Streaming parts — animation, never action
    Your output streams as PARTS while you write it — `text`, `thinking`, `call` — each
    `part_start {stream_id, source, kind, name?, target?, to?, re}` → `part_delta {text}`
    (batched 50 ms) → `part_end {ok}` | `part_abort {reason}`. A call part goes to its
    addressee's inbox (each of `to`) once `<target_id>`/`<to>` has closed; every other part
    goes on your own id, tagged with the caller's `client_id`. Deltas ride inboxes only —
    the state stream carries a part's start / end / abort, never a delta. Nothing
    dispatches from a part: a call without its `</call>` is not a call, and ONLY A
    SUCCESSFUL GENERATION DISPATCHES — a pass that stops with a block still open sends
    nothing, its earlier closed calls included. The message a call becomes carries the
    `stream_id` it streamed as (`· stream <id>` on the receiving header), so a consumer
    settles its draft on it; a call part that ended and saw no such message within 60 s
    is FAILED.

    A receiver stops you with `stop {stream_id?}`: the provider stream closes and the open
    parts abort. A CUT pass leaves a marker row on top of your context, never a half
    answer: `[stopped by <receiver>]` or `[model stopped generating]` (`… while thinking`
    when cut inside a thought), then the visible text, then `[a <tool> call to <target> was
    not sent]` for every call of the pass.

    `rows {after?, limit?}` is a VIEW of your conversation exactly as you hold it in
    memory — no store read, no transform; before your first turn you hold nothing
    (`loaded:false`).

    ## Recursion guard
    A `send` carrying a reserved `_call_stack` that already contains this agent (a cycle)
    or exceeds depth 8 is refused BEFORE the FIFO lock — so an authored agent's AI→AI chain can
    neither deadlock nor run away. Don't set `_call_stack` yourself.

    ## Meta-possibility — any routine orchestrates the whole substrate
    Every routine (this AI, a host job, a peer kernel, or any out-of-process caller)
    reaches every agent by id. From any of them you can read memory anywhere (`send(<state>,
    {read})`), run an inference turn (`send(<ai>, {send, ...})`), and/or spawn compute
    (`send(<py>, {start, code})`) — regardless of which kernel owns the target. Code
    steps and LLM-call steps are interchangeable, both directions.

    """#

    public func handle(
        agentId: AgentId,
        payload: JSON,
        kernel: Kernel
    ) async throws -> JSON? {
        try await core.handle(agentId: agentId, payload: payload, kernel: kernel)
    }

    /// The kernel is killing a task of this agent — close the turn out for the
    /// caller before the substrate cancels it.
    public func onCancel(agentId: AgentId, taskId: TaskId, kernel: Kernel) async throws {
        try await core.onCancel(agentId: agentId, taskId: taskId, kernel: kernel)
    }

    /// The shared delete hook: the deleted id's AI-core rows go with it.
    public func onDelete(agentId: AgentId, kernel: Kernel) async throws {
        await core.onDelete(agentId: agentId, kernel: kernel)
    }

    /// python `int(rec.get("max_tokens") or MAX_TOKENS)` — absent, null or 0 is the
    /// default.
    static func maxTokens(_ raw: JSON?) -> Int64 {
        guard let n = raw?.asInt, n != 0 else { return Int64(DEFAULT_MAX_TOKENS) }
        return n
    }

    /// The record's `sampling` object with null values dropped, in the record's key
    /// order. Anything that is not an object is empty.
    static func sampling(_ raw: JSON?) -> OrderedDictionary<String, JSON> {
        guard let obj = raw?.asObject else { return [:] }
        return obj.filter { !$0.value.isNull }
    }
}

/// Request-body keys this provider sets itself; `sampling` may not touch them.
let OWNED_KEYS: Set<String> = [
    "model", "input", "instructions", "max_output_tokens", "stream", "store",
]

/// Responses-API SSE provider — Bearer auth and a bounded retry for transient
/// GATEWAY failures (nothing else is retried).
struct ResponsesProvider: AIProvider {
    let endpoint: String
    let model: String
    /// `key(endpoint) -> key`, awaited before EACH request (`Credentials.swift`);
    /// a refused read is a raise inside the turn.
    let key: KeyReader
    let maxTokens: Int64
    /// The record's `sampling`, merged VERBATIM after the fixed fields.
    let sampling: OrderedDictionary<String, JSON>

    /// The keys this provider owns are refused, never silently overridden
    /// (audit d8 — python raises the same `ValueError` in its constructor).
    init(
        endpoint: String, model: String, key: @escaping KeyReader, maxTokens: Int64,
        sampling: OrderedDictionary<String, JSON>
    ) throws(PyError) {
        let owned = sampling.keys.filter { OWNED_KEYS.contains($0) }.sorted()
        if !owned.isEmpty {
            throw PyError(
                "ValueError",
                "responses_backend: `sampling` may not set \(owned.joined(separator: ", ")) — "
                    + "those are the record's (`model`, `max_tokens`) or the provider's own")
        }
        self.endpoint = endpoint
        self.model = model
        self.key = key
        self.maxTokens = maxTokens
        self.sampling = sampling
    }
    /// What the endpoint REPORTED for the last pass — the prompt size and the stop
    /// reason.
    let usage: AIUsage? = AIUsage()

    /// EDGE TRANSPORT FAILURES — not answers, and not statements about the
    /// request. A 4xx, or any 5xx carrying a JSON body, is the endpoint
    /// DELIBERATELY answering: that is the caller's bug and it stays loud. These
    /// codes with a NON-JSON body are a CDN/proxy error page, meaning the request
    /// never reached anything that could judge it. Retrying the same single path
    /// on a transport failure is not a second mechanism — nothing was tried and
    /// nothing is being routed around. Python `TRANSPORT_CODES`.
    static let transportCodes: Set<Int> = [502, 503, 504, 520, 521, 522, 523, 524]

    /// One client for every pass: connect bounded at 15 s, no read clock.
    static let client = HTTPClient(
        eventLoopGroupProvider: .singleton,
        configuration: .init(timeout: .init(connect: .seconds(15))))
    static let retries = 2
    static let backoff = 1.5

    /// The request body in PYTHON'S KEY ORDER — `model, input, max_output_tokens,
    /// stream, store`, then `instructions` when the chat carries a non-empty system
    /// block, then every `sampling` key last.
    func requestBody(messages: [JSON], maxTokens sideBound: Int? = nil) -> OrderedDictionary<String, JSON> {
        let (instructions, items) = translateResponsesMessages(messages)
        var body: OrderedDictionary<String, JSON> = [:]
        body["model"] = .string(model)
        body["input"] = .array(items)
        // A SIDE CALL'S OWN BOUND (a fold summary's budget) — else the record's.
        // python `max_tokens or self._max_tokens`: a 0 bound is no bound.
        body["max_output_tokens"] = .integer(
            sideBound.flatMap { $0 == 0 ? nil : Int64($0) } ?? maxTokens)
        body["stream"] = .bool(true)
        // STATELESS: the conversation is the agent's, never the endpoint's.
        body["store"] = .bool(false)
        if let instructions, !instructions.isEmpty { body["instructions"] = .string(instructions) }
        for (k, v) in sampling { body[k] = v }
        return body
    }

    func chat(messages: [JSON]) -> AsyncThrowingStream<AIChunk, Error> {
        chat(messages: messages, maxTokens: nil)
    }

    func chat(messages: [JSON], maxTokens sideBound: Int?) -> AsyncThrowingStream<AIChunk, Error> {
        let endpoint = endpoint
        let model = model
        let key = key
        let usage = usage
        let payload = JSON.object(requestBody(messages: messages, maxTokens: sideBound)).serialize()
        return AsyncThrowingStream { continuation in
            let task = Task {
                // python's order: the endpoint, then the model, then the key (read
                // while the headers are built — once per request, not per retry).
                if endpoint.isEmpty {
                    continuation.finish(
                        throwing: PyError(
                            "RuntimeError",
                            "responses_backend: no `endpoint` set — this connector does not "
                                + "guess a host. Name the full …/v1/responses URL on the agent "
                                + "record (endpoint=...)."))
                    return
                }
                if model.isEmpty {
                    continuation.finish(
                        throwing: PyError(
                            "RuntimeError",
                            "responses_backend: no `model` set — this connector does not "
                                + "guess one. Name a model on the agent record (model=...)."))
                    return
                }
                let apiKey: String
                do {
                    apiKey = try await key(endpoint) ?? ""
                } catch {
                    continuation.finish(throwing: error)
                    return
                }
                var attempt = 0
                while true {
                    var req = HTTPClientRequest(url: endpoint)
                    req.method = .POST
                    req.headers.add(name: "Content-Type", value: "application/json")
                    req.headers.add(name: "Authorization", value: "Bearer \(apiKey)")
                    req.headers.add(name: "Accept", value: "text/event-stream")
                    req.body = .bytes(ByteBuffer(string: payload))
                    do {
                        // NO READ CLOCK ON THE STREAM — the turn's one bound is ai_core's
                        // send timeout; a queued turn is not a dead one. Connect stays
                        // finite (15 s, the client's configuration). Python d8.
                        let response = try await Self.client.execute(req, deadline: .distantFuture)
                        let code = Int(response.status.code)
                        if code >= 400 {
                            let collected = try? await response.body.collect(upTo: 1 << 20)
                            let detail = collected.map { String(buffer: $0) } ?? ""
                            let ctype = response.headers.first(name: "content-type") ?? ""
                            if !(Self.transportCodes.contains(code)
                                && !ctype.lowercased().contains("json")
                                && attempt < Self.retries)
                            {
                                continuation.finish(
                                    throwing: PyError("RuntimeError", "responses \(code): \(detail)"))
                                return
                            }
                            attempt += 1
                            RuntimeLog.out(
                                "[responses] \(code) "
                                    + "(\(ctype.isEmpty ? "no content-type" : ctype)) — "
                                    + "transport error, retry \(attempt)/\(Self.retries)")
                        } else {
                            var state = ResponsesStreamState()
                            for try await line in bytesToLines(response.body) {
                                let d = decodeResponsesSSELine(line, state: &state)
                                if let n = d.promptTokens { usage?.lastPromptTokens = n }
                                for token in d.tokens { continuation.yield(.token(token)) }
                                if let err = d.error {
                                    continuation.finish(
                                        throwing: PyError("RuntimeError", err))
                                    return
                                }
                                if d.done { break }
                            }
                            // A span the stream never closed is balanced before the
                            // verdict, so a parser always reads a closed pair.
                            for token in state.wrap.finish() { continuation.yield(.token(token)) }
                            let stop = responsesStopReason(state)
                            usage?.lastStopReason = stop
                            // A BOUNDED SIDE CALL cut at its bound is an ANSWER.
                            if sideBound != nil && stop == "max_tokens" {
                                continuation.finish()
                                return
                            }
                            if let why = responsesTerminalityError(state) {
                                continuation.finish(throwing: PyError("RuntimeError", why))
                                return
                            }
                            continuation.finish()
                            return
                        }
                    } catch {
                        continuation.finish(throwing: error)
                        return
                    }
                    try? await Task.sleep(
                        nanoseconds: UInt64(Self.backoff * Double(attempt) * 1_000_000_000))
                }
            }
            continuation.onTermination = { _ in task.cancel() }
        }
    }
}
