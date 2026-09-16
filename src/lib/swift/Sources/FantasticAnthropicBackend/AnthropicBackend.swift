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

/// The API's own default host. An `endpoint` on the record overrides it — and
/// MUST, for any proxy: without it the record falls back here while carrying
/// whatever key the run exported, which is a 401 the moment that key belongs to
/// a different host.
public let DEFAULT_ENDPOINT = "https://api.anthropic.com/v1/messages"

/// The variables that carry the key, in python's order (`provider.py`).
/// Named in the failure text so an unwired connector says what to set.
let KEY_VARS = ["ANTHROPIC_KEY", "ANTHROPIC_API_KEY"]

/// Read the key from the environment. Nothing else is consulted: not the agent
/// record, not a sidecar, not settings.
///
/// Python's `os.environ.get("ANTHROPIC_KEY") or os.environ.get("ANTHROPIC_API_KEY")`
/// — an EMPTY variable is no key, and the value is used as it stands.
func readApiKey() -> String? {
    for v in KEY_VARS {
        if let raw = ProcessInfo.processInfo.environment[v], !raw.isEmpty { return raw }
    }
    return nil
}

/// What an unwired connector RAISES, inside the turn (`provider.py:112-119`).
/// Not a refusal body: python raises here, so the caller's settle reads
/// `{error:"RuntimeError: …", raised:true}` and the status event `str(e)`.
let MISSING_KEY_ERROR = "ANTHROPIC_KEY not set in environment (.env)"

/// NO guessy default (by default, no defaults): this connector never names a
/// model that reaches the network. The sentinel exists ONLY so `reflect` can say
/// what is true; `chat()` fails loud when the model is unset or still this value.
/// (py-canonical `anthropic_backend.provider.DEFAULT_MODEL`.)
public let DEFAULT_MODEL = "(no default — set the `model` meta)"
public let ANTHROPIC_VERSION = "2023-06-01"
/// `max_tokens` is REQUIRED by the Messages API — defaulted when unset.
public let DEFAULT_MAX_TOKENS = 8192

public final class AnthropicBundle: AgentBundle, @unchecked Sendable {
    public let name = "anthropic_backend"

    private let core: AIBackend

    public init() {
        self.core = buildAIBackend(
            AIBackendConfig(
                kind: "anthropic_backend",
                sentence:
                    "Anthropic-backed LLM agent (raw prompt-and-parse tool-calling).",
                defaultModel: DEFAULT_MODEL,
                defaultEndpoint: DEFAULT_ENDPOINT,
                // NO reflect extras: python's anthropic binding passes no
                // `reflect_extra`, and `model`/`endpoint` are base fields.
                makeProvider: { agent, _, _ in
                    // THE ENVIRONMENT IS THE ONLY SOURCE — python-canonical.
                    // The key used to be read off the agent's own meta, which
                    // means it was WRITTEN INTO agent.json on disk; rust and
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
                            endpoint: Self.endpoint(agent: agent),
                            model: Self.model(agent: agent),
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
        Talks to the Anthropic Messages API (default model `claude-opus-4-8`). Key read from `ANTHROPIC_KEY` (or `ANTHROPIC_API_KEY`) in the environment / `.env`. Per-client chat threads, FIFO lock, and automatic context management. **No agentic loop** — one provider pass per `send`. Persistence via `file_bridge_id`.

        ## Implementation
        This bundle is a thin binding over the shared `ai_core` lib. The queue / FIFO lock, prompt assembly, the ONE-PASS `_run`, and all the context machinery + verb bodies live in `ai_core.core`; this module supplies the `AnthropicProvider` builder (endpoint/model from the agent record) and `ai_core.build()` wires the rest into `(VERBS, handler)`.

        ## Bare completion — the tool loop is the CALLER's
        This backend does NOT parse tool calls and does NOT loop. `send` performs exactly ONE
        provider pass. It NEVER sends Anthropic `tools`/`input_schema` nor reads `tool_use`
        blocks; the `AnthropicProvider` streams plain `text_delta` content, returned verbatim.
        Any tool convention is the caller's — an authored agent teaches it in the prompt (via
        `system_prompt`), reads the completion, dispatches, and feeds the result back as the
        next `{send, text}` turn.

        ## Context management — owned HERE
        Like ollama_backend: per-client `chat.json` history, auto-compaction on overflow
        (`context_strategy`, default `compact`, + `[context-notice]`), and the `recall` /
        `context_status` verbs. The conversation's owner owns its context.

        ## Calling this agent
        `send {type:'send', text, client_id?}` runs ONE completion (per-backend FIFO lock)
        and returns `{response, final}`. A first-class peer of `exec_bridge`: a scheduler,
        a host job, a peer kernel, or a routine can call it by id. Optional `system_prompt:str`
        REPLACES the default system block (the caller supplies its own role / tool convention).
        Optional `messages:list` REPLACES persisted history (fully stateless — no compaction;
        the caller owns the context in that mode).

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

    fileprivate static func endpoint(agent: Agent) -> String {
        agent.metaValue(forKey: "endpoint")?.asString ?? DEFAULT_ENDPOINT
    }

    fileprivate static func model(agent: Agent) -> String {
        agent.metaValue(forKey: "model")?.asString ?? DEFAULT_MODEL
    }

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
    func requestBody(messages: [JSON]) -> OrderedDictionary<String, JSON> {
        let (system, turns) = translateAnthropicMessages(messages)
        var body: OrderedDictionary<String, JSON> = [:]
        body["model"] = .string(model)
        body["max_tokens"] = .integer(maxTokens)
        body["messages"] = .array(turns)
        body["stream"] = .bool(true)
        // `if system:` — an EMPTY system block is no `system` key at all.
        if let system, !system.isEmpty { body["system"] = .string(system) }
        for (k, v) in sampling { body[k] = v }
        return body
    }

    func chat(messages: [JSON]) -> AsyncThrowingStream<AIChunk, Error> {
        let endpoint = endpoint
        let model = model
        let apiKey = apiKey
        // RAW, always: no native `tools`/`tool_choice`.
        let payload = JSON.object(requestBody(messages: messages)).serialize()
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
                if model.isEmpty || model == DEFAULT_MODEL {
                    continuation.finish(
                        throwing: PyError(
                            "RuntimeError",
                            "anthropic_backend: no `model` set — this connector does not "
                                + "guess one. Name a model on the agent record (model=...)."))
                    return
                }
                // VERBATIM — `endpoint` IS the messages URL, as in python
                // (see DEFAULT_ENDPOINT). It used to be treated as a base with
                // `/messages` appended, so one meta key meant two things across
                // runtimes and a working proxy URL 404'd on the other kernel.
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
                                continuation.finish(
                                    throwing: PyError("RuntimeError", "anthropic \(code): \(detail)")
                                )
                                return
                            }
                            attempt += 1
                            FileHandle.standardOutput.write(
                                Data(
                                    ("[anthropic] \(code) "
                                        + "(\(ctype.isEmpty ? "no content-type" : ctype)) — "
                                        + "transport error, retry \(attempt)/\(Self.retries)\n")
                                        .utf8))
                        } else {
                            var state = AnthropicStreamState()
                            for try await line in bytesToLines(response.body) {
                                let d = decodeAnthropicSSELine(line, state: &state)
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
