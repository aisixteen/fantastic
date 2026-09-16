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

/// The NIM API's own host (py `nvidia_nim_backend.provider.DEFAULT_ENDPOINT`).
public let DEFAULT_ENDPOINT = "https://integrate.api.nvidia.com/v1"

/// The variable carrying the key. Named in the failure text so an unwired
/// connector says exactly what to set. Python `KEY_VAR`.
public let KEY_VAR = "NVIDIA_API_KEY"

/// The key, from the ENVIRONMENT. Nothing else is consulted — not a sidecar, and
/// NOT the agent record: a key on the record is a key written into `agent.json`
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

    let core: AIBackend

    public init() {
        self.core = buildAIBackend(
            AIBackendConfig(
                kind: "nvidia_nim_backend",
                sentence:
                    "NVIDIA NIM-backed LLM agent (OpenAI-compatible; raw prompt-and-parse tool-calling).",
                // NIM's `done` error path keeps `accumulated`.
                defaultModel: DEFAULT_MODEL,
                defaultEndpoint: DEFAULT_ENDPOINT,
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
                            endpoint: Self.endpoint(agent: agent),
                            model: Self.model(agent: agent),
                            apiKey: apiKey))
                }
            ))
    }

    public var readme: String? { Self.readmeText }

    /// THE PYTHON FILE, BYTE FOR BYTE (`nvidia_nim_backend/readme.md`). Persisted as
    /// `agents/<id>/readme.md` and served by `reflect readme=true`, so a swift
    /// kernel and a python one hand a reader the same document. A swift-written
    /// summary of it was a second, shorter truth about the same bundle.
    static let readmeText = #"""
        # nvidia_nim_backend — NVIDIA NIM, a context-managed completion agent
        OpenAI-compatible LLM backend. The api key comes from the ENVIRONMENT (`NVIDIA_API_KEY`) and is never stored; rate-limit retry; automatic context management. **No agentic loop** — one provider pass per `send`. Same surface as ollama_backend.

        ## Implementation
        This bundle is a thin binding over the shared `ai_core` lib. The queue / FIFO lock, prompt assembly, the ONE-PASS `_run`, and all the context machinery + verb bodies live in `ai_core.core`; this module supplies the `NvidiaNimProvider` builder and the NIM-specific extras below, and `ai_core.build()` wires the rest into `(VERBS, handler)`.

        ## Bare completion — the tool loop is the CALLER's
        This backend does NOT parse tool calls and does NOT loop. `send` performs exactly ONE
        provider pass. It NEVER sends the OpenAI `tools`/`tool_choice` fields nor reads
        `delta.tool_calls`; the `NvidiaNimProvider` streams plain `content` text, returned
        verbatim. Any tool convention is the caller's — an authored agent teaches it in the
        prompt (via `system_prompt`), reads the completion, dispatches, and feeds the result
        back as the next `{send, text}` turn.

        ## Context management — owned HERE
        Like ollama_backend: per-client `chat.json` history, auto-compaction on overflow
        (`context_strategy`, default `compact`, + `[context-notice]`), and the `recall` /
        `context_status` verbs. The conversation's owner owns its context.

        ## NIM-specific verbs
        - THE API KEY COMES FROM THE ENVIRONMENT (`NVIDIA_API_KEY`), read at call time. There is no key-storage verb and no sidecar: this bundle writes the key nowhere, so it is never in agent.json and never returned by reflect.

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
        yaml coupling). Optional `messages:list` REPLACES persisted history (fully
        stateless — though `api_key` is still required to reach the provider).

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

    // MARK: - meta helpers

    /// The record's `endpoint` — the OpenAI-compatible BASE, with `/chat/completions`
    /// appended by the provider (python `httpx.AsyncClient(base_url=…)`). It used
    /// to read a `host` meta and append `/v1/chat/completions`, so one runtime's
    /// working record was a 404 on the other.
    fileprivate static func endpoint(agent: Agent) -> String {
        agent.metaValue(forKey: "endpoint")?.asString ?? DEFAULT_ENDPOINT
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
    let endpoint: String
    let model: String
    let apiKey: String

    func chat(messages: [JSON]) -> AsyncThrowingStream<AIChunk, Error> {
        let endpoint = endpoint
        let model = model
        let apiKey = apiKey
        return AsyncThrowingStream { continuation in
            let task = Task {
                if model.isEmpty || model == DEFAULT_MODEL {
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
                        let decoded = decodeNimSSELine(line, wrap: &wrap)
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
