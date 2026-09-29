// Anthropic Messages-API LLM backend.
//
// Mirrors Rust's `fantastic-anthropic-backend::AnthropicBundle` and python's
// `anthropic_backend`. Talks to the Messages API via HTTPS POST + `x-api-key` +
// `anthropic-version`; streams RAW TEXT through event-typed Server-Sent Events.
//
// NO NATIVE TOOL-CALLING. Tool-calling here is raw prompt-and-parse owned by
// FantasticAICore: the provider streams text and the core parses the
// `<call name=…>` envelope out of it. The optional `tool_use` route that
// declared one `send` tool and translated the returned blocks back is DELETED —
// a model writing JSON tool calls is writing a grammar this kernel does not
// parse, and the honest answer is to say so (see the SSE decoder), not to
// translate a second dialect at the edge.
//
// All the agent machinery (history, epoch cancellation, verb dispatch,
// token/done events, envelope parsing) lives in the shared
// `FantasticAICore.AIBackend`. This file keeps only Anthropic-specific wire.

import AsyncHTTPClient
import FantasticAICore
import FantasticJSON
import FantasticKernel
import Foundation
import NIOCore
import OrderedCollections

public let HANDLER_MODULE = "anthropic_backend.tools"

/// The ONE variable that carries the key (`provider.py`). Named in the failure
/// text so an unwired connector says what to set.
let KEY_VAR = "ANTHROPIC_API_KEY"

/// Read the key from the environment. Nothing else is consulted: not the agent
/// record, not a sidecar, not settings.
///
/// Python's `os.environ.get("ANTHROPIC_API_KEY")` — an EMPTY variable is no key,
/// and the value is used as it stands.
func readApiKey() -> String? {
    guard let raw = ProcessInfo.processInfo.environment[KEY_VAR], !raw.isEmpty else {
        return nil
    }
    return raw
}

/// What an unwired connector RAISES, inside the turn (`provider.py:112-119`).
/// Not a refusal body: python raises here, so the caller's settle reads
/// `{error:"RuntimeError: …", raised:true}` and the status event `str(e)`.
let MISSING_KEY_ERROR = "\(KEY_VAR) not set in environment (.env)"

public let ANTHROPIC_VERSION = "2023-06-01"
/// `max_tokens` is REQUIRED by the Messages API — defaulted when unset.
public let DEFAULT_MAX_TOKENS = 8192

public final class AnthropicBundle: AgentBundle, @unchecked Sendable {
    public let name = "anthropic_backend"
    public var verbs: [String] { AIBackend.verbNames }

    let core: AIBackend

    public init() {
        self.core = buildAIBackend(
            AIBackendConfig(
                kind: "anthropic_backend",
                sentence:
                    "Anthropic-backed LLM agent (raw prompt-and-parse tool-calling).",
                // NO reflect extras: python's anthropic binding passes no
                // `reflect_extra`, and `model`/`endpoint` are base fields.
                makeProvider: { agent, _, _ in
                    // THE ENVIRONMENT IS THE ONLY SOURCE — python-canonical.
                    // The key used to be read off the agent's own meta, which
                    // means it was WRITTEN INTO agent.yaml on disk; rust and
                    // python's nvidia bundle both kept it out of the record for
                    // exactly that reason. Now nothing persists it at all.
                    //
                    // A MISSING KEY IS NOT A REFUSAL HERE. python builds the
                    // provider unconditionally and checks the variable inside
                    // `chat()`, so the turn reaches `status(thinking)` and then
                    // raises; refusing at construction skipped the event and
                    // answered a body python never sends.
                    .provider(
                        AnthropicProvider(
                            endpoint: try requiredMeta(agent, "endpoint", "anthropic_backend"),
                            model: try requiredMeta(agent, "model", "anthropic_backend"),
                            apiKey: readApiKey(),
                            maxTokens: agent.metaValue(forKey: "max_tokens")?.asInt
                                ?? Int64(DEFAULT_MAX_TOKENS),
                            // RUNNER (meta) params — sampling/budget knobs saved on
                            // the record, distinct from connection (endpoint/key/
                            // model). Merged verbatim into the request body.
                            sampling: Self.sampling(agent.metaValue(forKey: "sampling"))))
                }
            ))
    }

    public var readme: String? { Self.readmeText }

    /// THE PYTHON FILE, BYTE FOR BYTE (`anthropic_backend/readme.md`). Persisted as
    /// `agents/<id>/readme.md` and served by `reflect readme=true`, so a swift
    /// kernel and a python one hand a reader the same document. A swift-written
    /// summary of it was a second, shorter truth about the same bundle.
    static let readmeText = #"""
    # anthropic_backend — Claude, a context-managed completion agent
    Talks to the Anthropic Messages API. `endpoint` and `model` are REQUIRED on the record — nothing is guessed. Key read from `ANTHROPIC_API_KEY` in the environment / `.env`. Per-client chat threads, FIFO lock, and automatic context management. **No agentic loop** — one provider pass per `send`. Conversation in its memory store (`memory_store_id`).

    ## Implementation
    This bundle is a thin binding over the shared `ai_core` lib. The queue / FIFO lock, prompt assembly, the ONE-PASS `_run`, and all the context machinery + verb bodies live in `ai_core.core`; this module supplies the `AnthropicProvider` builder (endpoint/model from the agent record) and `ai_core.build()` wires the rest into `(VERBS, handler)`.

    ## Bare completion + the `<call>` grammar — no native tool API
    `send` performs exactly ONE provider pass: the `AnthropicProvider` streams plain `text_delta` content and never sends Anthropic `tools`/`input_schema` nor reads `tool_use` blocks. Tool-calling is RAW and shared by every backend: `ai_core` parses the model's `<call name=…>` tags from the streamed text, dispatches them as ONE batch (`kernel.send([...])`), and each destination's results arrive as ONE fresh turn — there is no loop inside a pass.

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
        // NO key-storage verbs: the key is read from the ENVIRONMENT at call
        // time, as in python and rust. A connector stays inert until the
        // operator wires the variable.
        return try await core.handle(agentId: agentId, payload: payload, kernel: kernel)
    }

    /// The kernel is killing a task of this agent — close the turn out for the
    /// caller before the substrate cancels it.
    public func onCancel(agentId: AgentId, taskId: TaskId, kernel: Kernel) async throws {
        await core.onCancel(agentId: agentId, taskId: taskId, kernel: kernel)
    }

    // MARK: - meta helpers

    /// The record's `sampling` object with null values dropped, in the record's
    /// key order — python's `{k: v for k, v in sampling.items() if v is not None}`.
    /// Anything that is not an object (unset, or a scalar) is empty.
    static func sampling(_ raw: JSON?) -> OrderedDictionary<String, JSON> {
        guard let obj = raw?.asObject else { return [:] }
        return obj.filter { !$0.value.isNull }
    }
}

/// Messages-API SSE provider — `x-api-key` auth, 429 retry with `retry-after`,
/// and a bounded retry for transient GATEWAY failures.
struct AnthropicProvider: AIProvider {
    let endpoint: String
    let model: String
    /// READ AT CONSTRUCTION, CHECKED IN `chat()` — nil when neither variable is
    /// set, which is a RAISE inside the turn, not a refusal before it.
    let apiKey: String?
    let maxTokens: Int64
    /// RUNNER (meta) PARAMS — the record's `sampling` object, merged VERBATIM
    /// into the request body after the fixed fields, exactly as python's
    /// provider does (`body.update(self._sampling)`). This is what bounds a
    /// reasoning model's deliberation (`thinking_token_budget`) and kills the
    /// CoT-repetition loop (`no_repeat_ngram_size`, `repetition_penalty`):
    /// without it a long swift turn ran past the harness's 180s send cap
    /// (measured 2026-09-15: E20 build failed 3/3).
    let sampling: OrderedDictionary<String, JSON>
    /// What the endpoint REPORTED for the last pass — the prompt size (ai_core
    /// calibrates its own estimate with it) and the stop reason.
    let usage: AIUsage? = AIUsage()

    /// 502/503/504/520-524 from a CDN in front of the API is a hiccup, not an
    /// answer: a 1-2s restart once returned a Cloudflare HTML page and killed a
    /// 26-pass run at pass 5. Retried ONLY when the body is not JSON — a JSON
    /// body is the API speaking deliberately, and re-sending a real refusal
    /// until the budget ran out would be worse than failing loudly.
    ///
    /// NOTHING ELSE IS RETRIED — a 429 included. python's anthropic provider has
    /// no rate-limit branch at all: the endpoint answered deliberately, the
    /// caller is over quota, and silently sleeping inside the turn burns the
    /// send ceiling while the caller sees a `thinking` phase that never ends.
    /// (The nvidia backend, whose free tier rate-limits by design, keeps its
    /// one retry — there, and only there.)
    static let transportCodes: Set<Int> = [502, 503, 504, 520, 521, 522, 523, 524]
    static let retries = 2
    static let backoff = 1.5

    /// The Messages-API request body in PYTHON'S KEY ORDER —
    /// `model, max_tokens, messages, stream`, then `system` when the chat carries
    /// a non-empty one, then every `sampling` key LAST in the record's order
    /// (python: `body.update(self._sampling)`) so an explicit knob wins over a
    /// fixed field of the same name. Swift emitted `stream` before `messages`,
    /// which is the same request and a different byte stream.
    func requestBody(messages: [JSON], maxTokens sideBound: Int? = nil) -> OrderedDictionary<String, JSON> {
        let (system, turns) = translateAnthropicMessages(messages)
        var body: OrderedDictionary<String, JSON> = [:]
        body["model"] = .string(model)
        // A SIDE CALL'S OWN BOUND (a fold summary's budget) — else the record's.
        body["max_tokens"] = .integer(sideBound.map { Int64($0) } ?? maxTokens)
        body["messages"] = .array(turns)
        body["stream"] = .bool(true)
        // `if system:` — an EMPTY system block is no `system` key at all.
        if let system, !system.isEmpty { body["system"] = .string(system) }
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
        // RAW, always: no native `tools`/`tool_choice`.
        let payload = JSON.object(requestBody(messages: messages, maxTokens: sideBound)).serialize()
        return AsyncThrowingStream { continuation in
            let task = Task {
                // THE KEY FIRST, THEN THE MODEL — python's order inside `chat()`
                // (`provider.py:112-119`), so an agent missing both is told
                // about the variable it has to export rather than about a model
                // it cannot reach anyway.
                guard let apiKey, !apiKey.isEmpty else {
                    continuation.finish(throwing: PyError("RuntimeError", MISSING_KEY_ERROR))
                    return
                }
                if model.isEmpty {
                    continuation.finish(
                        throwing: PyError(
                            "RuntimeError",
                            "anthropic_backend: no `model` set — this connector does not "
                                + "guess one. Name a model on the agent record (model=...)."))
                    return
                }
                // VERBATIM — `endpoint` IS the messages URL, as in python.
                let url = endpoint

                var attempt = 0

                while true {
                    var req = HTTPClientRequest(url: url)
                    req.method = .POST
                    req.headers.add(name: "Content-Type", value: "application/json")
                    req.headers.add(name: "x-api-key", value: apiKey)
                    req.headers.add(name: "anthropic-version", value: ANTHROPIC_VERSION)
                    req.headers.add(name: "Accept", value: "text/event-stream")
                    req.body = .bytes(ByteBuffer(string: payload))

                    do {
                        let response = try await HTTPClient.shared.execute(
                            req, timeout: .seconds(600))
                        let code = Int(response.status.code)

                        if code >= 400 {
                            // THE BODY IS THE ANSWER. python raises with the
                            // status AND what the endpoint said; swift threw a
                            // bare `send: provider HTTP <code>` and dropped the
                            // one sentence naming the cause.
                            let collected = try? await response.body.collect(upTo: 1 << 20)
                            let detail = collected.map { String(buffer: $0) } ?? ""
                            let ctype = response.headers.first(name: "content-type") ?? ""
                            if !(Self.transportCodes.contains(code)
                                && !ctype.lowercased().contains("json")
                                && attempt < Self.retries)
                            {
                                let why = "anthropic \(code): \(detail)"
                                continuation.finish(throwing: PyError("RuntimeError", why))
                                return
                            }
                            attempt += 1
                            // A15.4: python's `print(..., flush=True)` — routed
                            // through `RuntimeLog.out` (not a raw stdout write)
                            // so a test can read what an operator would.
                            RuntimeLog.out(
                                "[anthropic] \(code) "
                                    + "(\(ctype.isEmpty ? "no content-type" : ctype)) — "
                                    + "transport error, retry \(attempt)/\(Self.retries)")
                        } else {
                            var state = AnthropicStreamState()
                            for try await line in bytesToLines(response.body) {
                                let d = decodeAnthropicSSELine(line, state: &state)
                                if let n = d.promptTokens { usage?.lastPromptTokens = n }
                                for token in d.tokens { continuation.yield(.token(token)) }
                                // python RAISES from inside the loop, so the
                                // half-open think span is NOT closed on this
                                // path — the turn ends with the error, not with
                                // a tidy marker.
                                if let err = d.error {
                                    continuation.finish(throwing: PyError("RuntimeError", err))
                                    return
                                }
                                if d.done { break }
                            }
                            // AT END OF BODY, BEFORE TERMINALITY — a stream cut
                            // mid-thought still closes its `<think>` span, so the
                            // marker pair a parser reads is always balanced. Closing
                            // it only on `message_stop` left the span open on exactly
                            // the turns that were cut.
                            for token in state.wrap.finish() { continuation.yield(.token(token)) }
                            usage?.lastStopReason = state.stopReason
                            // A BOUNDED SIDE CALL that stopped at its bound is an
                            // ANSWER, not a fault: the caller asked for at most
                            // `max_tokens` and reads the stop reason (a fold refuses
                            // a cut summary).
                            if sideBound != nil && state.stopReason == "max_tokens" {
                                continuation.finish()
                                return
                            }
                            if let why = anthropicTerminalityError(state) {
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
                    // `BACKOFF * attempt` — 1.5s, then 3.0s, exactly as python
                    // sleeps at the bottom of its retry loop.
                    try? await Task.sleep(
                        nanoseconds: UInt64(Self.backoff * Double(attempt) * 1_000_000_000))
                }
            }
            continuation.onTermination = { _ in task.cancel() }
        }
    }
}
