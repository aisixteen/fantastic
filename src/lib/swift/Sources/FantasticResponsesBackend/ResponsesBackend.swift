// OpenAI Responses-API LLM backend.
//
// Mirrors python's `responses_backend` (canonical) and Rust's
// `fantastic-responses-backend`. POSTs the full `…/v1/responses` URL named on the
// record with `Authorization: Bearer $OPENAI_API_KEY`; streams RAW TEXT through the
// Responses event-typed Server-Sent Events.
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

/// NO guessed host: the sentinel `reflect` shows until the record names the full
/// `…/v1/responses` URL. `chat()` fails loud naming `endpoint` while it is unset.
public let DEFAULT_ENDPOINT = "(no default — set the `endpoint` meta: the full …/v1/responses URL)"

/// NO guessed model — python `responses_backend.provider.DEFAULT_MODEL`.
public let DEFAULT_MODEL = "(no default — set the `model` meta)"

/// The ONE variable the key is read from. No second spelling.
public let KEY_VAR = "OPENAI_API_KEY"

/// Per-turn output budget (`max_output_tokens`) when the record names none.
public let DEFAULT_MAX_TOKENS = 8192

/// Read the key from the environment. An EMPTY variable is no key.
func readApiKey() -> String? {
    guard let raw = ProcessInfo.processInfo.environment[KEY_VAR], !raw.isEmpty else { return nil }
    return raw
}

public final class ResponsesBundle: AgentBundle, @unchecked Sendable {
    public let name = "responses_backend"
    public var verbs: [String] { AIBackend.verbNames }

    private let core: AIBackend

    public init() {
        self.core = buildAIBackend(
            AIBackendConfig(
                kind: "responses_backend",
                sentence:
                    "OpenAI Responses-API LLM agent (raw prompt-and-parse tool-calling).",
                defaultModel: DEFAULT_MODEL,
                defaultEndpoint: DEFAULT_ENDPOINT,
                // NO reflect extras: python's binding passes no `reflect_extra`.
                makeProvider: { agent, _, _ in
                    // Built unconditionally from the record; the key is checked
                    // INSIDE `chat()`, so an unwired connector reaches
                    // `status(thinking)` and then raises — python's order.
                    .provider(
                        ResponsesProvider(
                            endpoint: agent.metaValue(forKey: "endpoint")?.asString
                                ?? DEFAULT_ENDPOINT,
                            model: agent.metaValue(forKey: "model")?.asString ?? DEFAULT_MODEL,
                            apiKey: readApiKey(),
                            maxTokens: Self.maxTokens(agent.metaValue(forKey: "max_tokens")),
                            sampling: Self.sampling(agent.metaValue(forKey: "sampling"))))
                }
            ))
    }

    public var readme: String? { Self.readmeText }

    /// THE PYTHON FILE, BYTE FOR BYTE (`responses_backend/readme.md`).
    static let readmeText = #"""
    # responses_backend — an OpenAI Responses-API, context-managed completion agent
    Talks to any endpoint that serves the OpenAI Responses API (`POST …/v1/responses`): OpenAI itself, Ollama ≥ 0.13.4, or a proxy in front of an open-weights model. `endpoint` (the full `…/v1/responses` URL) and `model` are REQUIRED on the record — nothing is guessed; the key is read from `OPENAI_API_KEY` in the environment / `.env`. Per-client chat threads, FIFO lock, and automatic context management. **No agentic loop** — one provider pass per `send`. Conversation in its memory store (`memory_store_id`).

    ## Implementation
    This bundle is a thin binding over the shared `ai_core` lib. The queue / FIFO lock, prompt assembly, the ONE-PASS `_run`, and all the context machinery + verb bodies live in `ai_core.core`; this module supplies the `ResponsesProvider` builder (endpoint/model from the agent record) and `ai_core.build()` wires the rest into `(VERBS, handler)`.

    ## The wire
    Stateless: every pass sends `store:false` and the whole conversation as `input` items (the system block as `instructions`) — the conversation is this agent's, never the endpoint's. The stream is read on BOTH channels: `response.reasoning_text` / `reasoning_summary_text` deltas are wrapped in `<think>…</think>` (parsed for calls, stripped before the turn is stored), `response.output_text` deltas are the output. A turn ends `completed`; one cut at `max_output_tokens` (`response.incomplete`) is a loud error on a main turn and an answer (`last_stop_reason:"max_tokens"`) on a bounded side call such as a fold summary. Sampling knobs on the record (`sampling`) merge verbatim into the request body.

    ## Bare completion — the tool loop is the CALLER's
    This backend does NOT parse tool calls and does NOT loop. `send` performs exactly ONE
    provider pass. It NEVER sends Responses `tools` nor reads `function_call` items (one
    arriving is a loud error); the stream's text is returned verbatim. Any tool convention
    is the caller's — an authored agent teaches it in the prompt (via `system_prompt`),
    reads the completion, dispatches, and feeds the result back as the next `{send, text}`
    turn.

    ## Context management — owned HERE
    The conversation lives in this agent's MEMORY STORE — `memory_store_id` on the record
    is REQUIRED (a `send` fails loud naming it): every turn, fold and view is a row under
    this agent's key. Reply BODIES are never stored — a reply row keeps its one-line
    `origin`, and after a restart the model re-reads from it. Compaction fires at 0.8 of
    the budget (calibrated by the endpoint's reported prompt size when it reports one)
    with the per-agent `context_strategy` (default `compact`; `fold`, `truncate`) and a
    `[context-notice]`; `context_status`, `context_recall`, `history {fold?}`, `grep` and
    `context_clear` (a boundary, nothing deleted) inspect and page it. Sharing is by
    GRANT, by the owner's own choice: it `share`s a scope of its rows through the store;
    another agent starts from it with `fork_from {owner, grant, message}` and asks for a
    third agent's row with `request_share`. `watch {body?}` / `unwatch` subscribe to this
    agent's row events (a digest; bodies only for a watcher the rows are shared with).
    ## Calling this agent
    `send {type:'send', text, client_id?}` runs ONE completion (per-backend FIFO lock)
    and returns `{response, final}`. A first-class peer of `exec_bridge`: a scheduler,
    a host job, a peer kernel, or a routine can call it by id. Optional `system_prompt:str`
    REPLACES the default system block (the caller supplies its own role / tool convention).
    There is no caller-supplied history: a one-off completion is a disposable agent with its own store; context for another agent travels in the text, or by grant (`share` → `fork_from`).

    ## Streaming
    You stream `token`/`status`/`done` on your OWN id. Any caller that `watch`es your id
    consumes the same stream (the driving caller, a browser tab, a peer). You do not route
    your own output — you complete; the caller reads it and decides what to do next.

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
        await core.onCancel(agentId: agentId, taskId: taskId, kernel: kernel)
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

/// Responses-API SSE provider — Bearer auth and a bounded retry for transient
/// GATEWAY failures (the anthropic provider's rule; nothing else is retried).
struct ResponsesProvider: AIProvider {
    let endpoint: String
    let model: String
    /// Read at construction, CHECKED in `chat()` — nil is a raise inside the turn.
    let apiKey: String?
    let maxTokens: Int64
    /// The record's `sampling`, merged VERBATIM after the fixed fields.
    let sampling: OrderedDictionary<String, JSON>
    /// What the endpoint REPORTED for the last pass — the prompt size and the stop
    /// reason.
    let usage: AIUsage? = AIUsage()

    static let transportCodes: Set<Int> = [502, 503, 504, 520, 521, 522, 523, 524]
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
        let apiKey = apiKey
        let usage = usage
        let payload = JSON.object(requestBody(messages: messages, maxTokens: sideBound)).serialize()
        return AsyncThrowingStream { continuation in
            let task = Task {
                // python's order: the key, then the endpoint, then the model.
                guard let apiKey, !apiKey.isEmpty else {
                    continuation.finish(
                        throwing: PyError("RuntimeError", "\(KEY_VAR) not set in environment (.env)"))
                    return
                }
                if endpoint.isEmpty || endpoint == DEFAULT_ENDPOINT {
                    continuation.finish(
                        throwing: PyError(
                            "RuntimeError",
                            "responses_backend: no `endpoint` set — this connector does not "
                                + "guess a host. Name the full …/v1/responses URL on the agent "
                                + "record (endpoint=...)."))
                    return
                }
                if model.isEmpty || model == DEFAULT_MODEL {
                    continuation.finish(
                        throwing: PyError(
                            "RuntimeError",
                            "responses_backend: no `model` set — this connector does not "
                                + "guess one. Name a model on the agent record (model=...)."))
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
                        let response = try await HTTPClient.shared.execute(req, timeout: .seconds(600))
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
                                    continuation.finish(throwing: PyError("RuntimeError", err))
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
