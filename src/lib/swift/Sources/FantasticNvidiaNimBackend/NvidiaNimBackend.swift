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

/// The variable carrying the key. Named in the failure text so an unwired
/// connector says exactly what to set. Python `KEY_VAR`.
public let KEY_VAR = "NVIDIA_API_KEY"

/// The key, from the ENVIRONMENT. Nothing else is consulted — not a sidecar, and
/// NOT the agent record: a key on the record is a key written into `agent.yaml`
/// on disk, which is how it leaks through a reflect or a store copy.
func readApiKey() -> String? {
    let raw = pyStrip(ProcessInfo.processInfo.environment[KEY_VAR] ?? "")
    return raw.isEmpty ? nil : raw
}

/// Free tier of NIM rate-limits at ~40 RPM/model. On a PRE-STREAM 429 the turn
/// waits `Retry-After` (clamped) and retries ONCE — capped so a misbehaving
/// server cannot pin the turn inside the send ceiling. Python `RATE_LIMIT_*`.
let RATE_LIMIT_MAX_WAIT = 60
let RATE_LIMIT_DEFAULT_WAIT = 5
let RATE_LIMIT_MAX_RETRIES = 1

/// Read `Retry-After` (seconds), clamp to `[1, RATE_LIMIT_MAX_WAIT]`. Anything
/// unparseable is the default wait — python's `int(val)` inside a try.
func parseRetryAfter(_ value: String?) -> Int {
    guard let n = try? pyInt(.string(value ?? "")) else { return RATE_LIMIT_DEFAULT_WAIT }
    if n < 1 { return 1 }
    return min(n, RATE_LIMIT_MAX_WAIT)
}

public final class NvidiaNimBundle: AgentBundle, @unchecked Sendable {
    public let name = "nvidia_nim_backend"
    public var verbs: [String] { AIBackend.verbNames }

    let core: AIBackend

    public init() {
        self.core = buildAIBackend(
            AIBackendConfig(
                kind: "nvidia_nim_backend",
                sentence:
                    "NVIDIA NIM-backed LLM agent (OpenAI-compatible; raw prompt-and-parse tool-calling).",
                // NIM's `done` error path keeps `accumulated`.
                // A PROVIDER HTTP FAILURE IS A CLEAN REFUSAL HERE, not a raise:
                // the free tier rate-limits by design and the caller can act on
                // being told so. Python `_map_error`.
                errorMapper: { error in
                    guard let e = error as? NimError, case .http(let code, let retryAfter) = e
                    else { return nil }
                    if code == 429 {
                        return "send: rate limited (429); retry in \(parseRetryAfter(retryAfter))s"
                    }
                    return "send: provider HTTP \(code)"
                },
                // RETRY ONCE on a PRE-STREAM 429, surfacing the wait as
                // `status(thinking, waiting_on:'rate_limit', wait_s)`. Python
                // `_stream_with_rate_limit_retry`.
                streamWrapper: { provider, messages, emitStatus in
                    nimRateLimitRetry(provider: provider, messages: messages, emit: emitStatus)
                },
                // `has_api_key` ONLY — never the key, and no `host`: the record's
                // `endpoint` is the base field that names where this goes.
                reflectExtra: { _ in ["has_api_key": .bool(readApiKey() != nil)] },
                // python `require_provider=True`: no key ⇒ refused before the queue.
                requireProvider: true,
                makeProvider: { agent, clientId, _ in
                    guard let apiKey = readApiKey() else {
                        return .refused(
                            .object([
                                "error": .string("nvidia_nim_backend: no api key — set \(KEY_VAR)"),
                                "client_id": .string(clientId),
                            ]))
                    }
                    return .provider(
                        NvidiaNimProvider(
                            endpoint: try requiredMeta(agent, "endpoint", "nvidia_nim_backend"),
                            model: try requiredMeta(agent, "model", "nvidia_nim_backend"),
                            apiKey: apiKey))
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
                "endpoint", "str", "the NIM OpenAI-compatible URL", required: true),
            RecordSchema.field("model", "str", "the model name", required: true),
        ],
        keyVar: KEY_VAR)

    /// THE PYTHON FILE, BYTE FOR BYTE (`nvidia_nim_backend/readme.md`). Persisted as
    /// `agents/<id>/readme.md` and served by `reflect readme=true`, so a swift
    /// kernel and a python one hand a reader the same document. A swift-written
    /// summary of it was a second, shorter truth about the same bundle.
    static let readmeText = #"""
    # nvidia_nim_backend — NVIDIA NIM, a context-managed completion agent
    OpenAI-compatible LLM backend. The api key comes from the ENVIRONMENT (`NVIDIA_API_KEY`) and is never stored; rate-limit retry; automatic context management. **No agentic loop** — one provider pass per `send`. Same surface as ollama_backend.

    ## Implementation
    This bundle is a thin binding over the shared `ai_core` lib. The queue / FIFO lock, prompt assembly, the ONE-PASS `_run`, and all the context machinery + verb bodies live in `ai_core.core`; this module supplies the `NvidiaNimProvider` builder and the NIM-specific extras below, and `ai_core.build()` wires the rest into `(VERBS, handler)`.

    ## Bare completion + the `<call>` grammar — no native tool API
    `send` performs exactly ONE provider pass: the `NvidiaNimProvider` streams plain `content` text and never sends the OpenAI `tools`/`tool_choice` fields nor reads `delta.tool_calls`. Tool-calling is RAW and shared by every backend: `ai_core` parses the model's `<call name=…>` tags from the streamed text, dispatches them as ONE batch (`kernel.send([...])`), and each destination's results arrive as ONE fresh turn — there is no loop inside a pass.

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
    ## NIM-specific verbs
    - THE API KEY COMES FROM THE ENVIRONMENT (`NVIDIA_API_KEY`), read at call time. There is no key-storage verb and no sidecar: this bundle writes the key nowhere, so it is never in agent.yaml and never returned by reflect.

    `reflect` reports `has_api_key:bool` (never the key value itself).

    ## Rate-limit and failfast behavior
    - **No key → failfast**: with `NVIDIA_API_KEY` unset, `send` returns an error immediately, NAMING the variable (`"no api key — set NVIDIA_API_KEY"`). The provider builder returns `None` when no key is present, and `ai_core` with `require_provider=True` surfaces this as a clean caller-facing error.
    - **HTTP 429 → retry once**: on a rate-limit response received before any chunk is yielded, the bundle waits `Retry-After` seconds (clamped to [1, 60]; default 5s if the header is absent) and retries the request once. Mid-stream 429 or any non-429 HTTP error propagates unchanged.

    ## Calling this agent as a workflow unit
    `send {type:'send', text, client_id?}` runs ONE inference turn (per-backend FIFO
    lock) and returns `{response, final}`. It is a first-class peer of
    `exec_bridge`: a scheduler, a host job, a peer kernel, or another AI can
    call it by id. Optional `system_prompt:str` REPLACES the auto-built prompt
    (caller-supplied role/context — read it from a state agent yourself; the AI has NO
    yaml coupling). There is no caller-supplied history: a one-off completion is a disposable agent with its own store; context for another agent travels in the text, or by grant (`share` → `fork_from`).

    ## Streaming
    You stream `token`/`status`/`done` on your OWN id. Any caller that `watch`es your id
    consumes the same stream (the driving caller, a browser tab filtering by `client_id`, a
    peer). You do not route your own output — you complete; the caller reads it and decides
    what to do next.

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

    /// The kernel is killing a task of this agent. If it is the in-flight
    /// generation, close the turn out for the caller before the substrate
    /// cancels it (which drops the SSE stream + releases the FIFO lock on its
    /// own). There is no `interrupt` verb any more: cancellation is the kernel's.
    public func onCancel(agentId: AgentId, taskId: TaskId, kernel: Kernel) async throws {
        await core.onCancel(agentId: agentId, taskId: taskId, kernel: kernel)
    }

}

/// NIM `/v1/chat/completions` SSE provider — Bearer auth, 429 retry with
/// exponential backoff (1s, 2s, 4s). PURE RAW TEXT: yields `.token`s only,
/// decoded from BOTH of the model's channels (`reasoning_content` wrapped in
/// `<think>…</think>`, `content` bare — see `decodeOpenAISSELine`); no native
/// tool-calls (ai-core parses the `<call name=…>` envelope from the text).
struct NvidiaNimProvider: AIProvider {
    let endpoint: String
    let model: String
    let apiKey: String
    /// What the endpoint REPORTED for the last pass — the prompt size (ai_core
    /// calibrates its own estimate with it) and the stop reason.
    let usage: AIUsage? = AIUsage()

    func chat(messages: [JSON]) -> AsyncThrowingStream<AIChunk, Error> {
        chat(messages: messages, maxTokens: nil)
    }

    func chat(messages: [JSON], maxTokens: Int?) -> AsyncThrowingStream<AIChunk, Error> {
        let endpoint = endpoint
        let model = model
        let apiKey = apiKey
        let usage = usage
        return AsyncThrowingStream { continuation in
            let task = Task {
                if model.isEmpty {
                    // RAISES, unmapped: python's `RuntimeError` here is a
                    // misconfiguration, not a provider refusal, and the caller's
                    // settle says `raised: true` so it reads as one.
                    continuation.finish(
                        throwing: PyError(
                            "RuntimeError",
                            "nvidia_nim_backend: no `model` set — this connector does not "
                                + "guess one. Name a model on the agent record (model=...)."))
                    return
                }
                var base = endpoint
                while base.hasSuffix("/") { base.removeLast() }
                var req = HTTPClientRequest(url: "\(base)/chat/completions")
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
                // PROVIDER-REPORTED USAGE on the last chunk (`usage.prompt_tokens`).
                body["stream_options"] = .object(["include_usage": .bool(true)])
                // A SIDE CALL'S OWN BOUND (a fold summary's budget).
                if let maxTokens { body["max_tokens"] = .integer(Int64(maxTokens)) }
                req.body = .bytes(ByteBuffer(string: JSON.object(body).serialize()))

                // NO RETRY LOOP HERE. `raise_for_status()` is all python's
                // provider does; the ONE retry lives in the stream wrapper,
                // which is the only place that can tell a PRE-STREAM 429 from a
                // mid-stream one and the only place that can say it is waiting.
                // The loop here could also fall out of three 429s and
                // `finish()` EMPTY — a successful turn that produced nothing.
                do {
                    let response = try await HTTPClient.shared.execute(req, timeout: .seconds(600))
                    let statusCode = Int(response.status.code)
                    if statusCode >= 400 {
                        continuation.finish(
                            throwing: NimError.http(
                                statusCode, response.headers.first(name: "retry-after")))
                        return
                    }
                    // ONE wrapper for the whole turn — the reasoning markers
                    // open and close once, not per chunk (`ThinkingWrap`).
                    var wrap = ThinkingWrap()
                    for try await line in bytesToLines(response.body) {
                        if let usage { recordOpenAIReport(line, into: usage) }
                        let decoded = decodeOpenAISSELine(line, wrap: &wrap)
                        for token in decoded.tokens { continuation.yield(.token(token)) }
                        if decoded.done { break }
                    }
                    // The model spent the tail of its turn (or all of it)
                    // reasoning: close the span so the text a reader parses
                    // stays balanced.
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

/// Retry ONCE on a PRE-STREAM 429 — python `_stream_with_rate_limit_retry`.
///
/// Mid-stream (rare: quota is usually checked up front) or any non-429 failure
/// propagates unchanged: the attempt already produced tokens the caller has
/// seen, and re-running it would duplicate them.
func nimRateLimitRetry(
    provider: any AIProvider, messages: [JSON], emit: AIStatusEmitter
) -> AsyncThrowingStream<AIChunk, Error> {
    AsyncThrowingStream { continuation in
        let task = Task {
            var attempt = 0
            while true {
                var yieldedAnything = false
                do {
                    for try await chunk in provider.chat(messages: messages) {
                        yieldedAnything = true
                        continuation.yield(chunk)
                    }
                    continuation.finish()
                    return
                } catch {
                    guard case .http(429, let retryAfter)? = error as? NimError,
                        !yieldedAnything, attempt < RATE_LIMIT_MAX_RETRIES
                    else {
                        continuation.finish(throwing: error)
                        return
                    }
                    attempt += 1
                    let wait = parseRetryAfter(retryAfter)
                    await emit(
                        "thinking",
                        [("waiting_on", .string("rate_limit")), ("wait_s", .integer(Int64(wait)))])
                    try? await Task.sleep(nanoseconds: UInt64(wait) * 1_000_000_000)
                }
            }
        }
        continuation.onTermination = { _ in task.cancel() }
    }
}

/// What the NIM wire can fail with. `http` carries the raw `Retry-After` header
/// so the wrapper and the error mapper read the SAME value the server sent.
enum NimError: Error, CustomStringConvertible {
    case http(Int, String?)
    var description: String {
        switch self {
        case .http(let code, _): return "send: provider HTTP \(code)"
        }
    }
}
