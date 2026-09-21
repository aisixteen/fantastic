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

/// NO guessy default: this connector never names a model that reaches the
/// network. The sentinel exists ONLY so `reflect` can say what is true.
/// (py-canonical `ollama_backend.provider.DEFAULT_MODEL`.)
public let DEFAULT_MODEL = "(no default — set the `model` meta)"

/// Where a local ollama listens by default (py `ollama_backend.provider`).
public let DEFAULT_ENDPOINT = "http://localhost:11434"

public final class OllamaBackendBundle: AgentBundle, @unchecked Sendable {
    public let name = "ollama_backend"
    public var verbs: [String] { AIBackend.verbNames }

    let core: AIBackend

    public init() {
        self.core = buildAIBackend(
            AIBackendConfig(
                kind: "ollama_backend",
                sentence: "Ollama-backed LLM agent (raw prompt-and-parse tool-calling).",
                defaultModel: DEFAULT_MODEL,
                defaultEndpoint: DEFAULT_ENDPOINT,
                // The SAMPLING KNOBS, in python's order — `model` and `endpoint`
                // are base fields now. `temperature` falls back to the provider's
                // 0.3; the rest are null unless the record sets them.
                reflectExtra: { agent in
                    [
                        "temperature": .double(Self.temperature(agent: agent)),
                        "num_ctx": Self.numCtx(agent: agent).map(JSON.integer) ?? .null,
                        "num_predict": Self.numPredict(agent: agent).map(JSON.integer) ?? .null,
                        "think": Self.think(agent: agent).map(JSON.bool) ?? .null,
                    ]
                },
                makeProvider: { agent, _, _ in
                    // AN UNSET MODEL IS NOT A REFUSAL BEFORE THE TURN. python
                    // builds the provider with the SENTINEL and raises inside
                    // `chat()`, so the turn reaches `status(thinking)` and the
                    // failure is the provider's, with python's own wording —
                    // swift refused early with a sentence of its own invention.
                    .provider(
                        OllamaProvider(
                            endpoint: Self.endpoint(agent: agent),
                            model: Self.model(agent: agent),
                            temperature: Self.temperature(agent: agent),
                            numCtx: Self.numCtx(agent: agent),
                            numPredict: Self.numPredict(agent: agent),
                            think: Self.think(agent: agent)))
                }
            ))
    }

    public var readme: String? { Self.readmeText }

    /// THE PYTHON FILE, BYTE FOR BYTE (`ollama_backend/readme.md`). Persisted as
    /// `agents/<id>/readme.md` and served by `reflect readme=true`, so a swift
    /// kernel and a python one hand a reader the same document. A swift-written
    /// summary of it was a second, shorter truth about the same bundle.
    static let readmeText = #"""
        # ollama_backend — local LLM, a context-managed completion agent
        Talks to a local ollama. Per-client chat threads, FIFO lock, and automatic
        context management (compaction / recall / context_status). Persistence via
        `file_bridge_id`. **No agentic loop** — one provider pass per `send`.

        ## Implementation
        A thin binding over the shared `ai_core` lib. The queue / FIFO lock, prompt
        assembly, the ONE-PASS `_run`, and all the context machinery live in
        `ai_core.core`; this module supplies the `OllamaProvider` builder (endpoint /
        model from the agent record) and `ai_core.build()` wires the rest into
        `(VERBS, handler)`.

        ## Bare completion — the tool loop is the CALLER's
        This backend does NOT parse tool calls and does NOT loop. `send` performs exactly
        ONE provider pass: it streams the model's pure text and returns it. Any tool
        convention is the caller's: an authored agent teaches its convention in the
        prompt (via `system_prompt`), reads the completion, dispatches, and feeds the
        result back as the next `{send, text}` turn. The `OllamaProvider` never receives a
        `tools` array and no `message.tool_calls` are read — model-agnostic.

        ## Context management — owned HERE (the conversation's owner owns its context)
        Because this backend owns the per-client conversation, it owns the context:
        - **history** — every `send` loads + appends the per-client `chat.json` thread;
        - **auto-compaction** — when the assembled prompt overflows the model's window,
          the configured `context_strategy` (default `compact`) summarizes / elides older
          turns ONCE at turn entry and inserts a `[context-notice]`; the durable
          transcript is never trimmed;
        - **`recall {query?, limit?}`** — page dropped turns back from the durable store;
        - **`context_status`** — inspect budget + last compaction + the model's reaction.

        ## Calling this agent
        `send {type:'send', text, client_id?}` runs ONE completion (per-backend FIFO lock)
        and returns `{response, final}`. A first-class peer of `exec_bridge`: a
        scheduler, a host job, a peer kernel, or a routine can call it by id. Optional
        `system_prompt:str` REPLACES the default system block (the caller supplies its own
        role / tool convention / whatever menu it built). Optional `messages:list`
        REPLACES persisted history (fully stateless — no `file_bridge_id` needed, and no
        compaction — the caller owns the context in that mode).

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
        await core.onCancel(agentId: agentId, taskId: taskId, kernel: kernel)
    }

    // MARK: - meta helpers

    /// The record's `endpoint` — where ollama listens, with `/api/chat` appended
    /// by the provider (python `rec.get("endpoint", DEFAULT_ENDPOINT)`). It used
    /// to read a `host` meta and ignore `endpoint` entirely, so a record that
    /// pointed python at another box left swift talking to 127.0.0.1.
    fileprivate static func endpoint(agent: Agent) -> String {
        agent.metaValue(forKey: "endpoint")?.asString ?? DEFAULT_ENDPOINT
    }

    /// The model id from the record's `model` meta, or the SENTINEL. No default
    /// is guessed: a bad guess reaches the network (a wrong model 404s), and the
    /// sentinel is what `chat()` fails loud on.
    fileprivate static func model(agent: Agent) -> String {
        agent.metaValue(forKey: "model")?.asString ?? DEFAULT_MODEL
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
/// `<call name=…>` envelope from the text. Mirrors Rust/Python ollama providers.
struct OllamaProvider: AIProvider {
    let endpoint: String
    let model: String
    let temperature: Double
    let numCtx: Int64?
    let numPredict: Int64?
    let think: Bool?

    func chat(messages: [JSON]) -> AsyncThrowingStream<AIChunk, Error> {
        let endpoint = endpoint
        let model = model
        let temperature = temperature
        let numCtx = numCtx
        let numPredict = numPredict
        let think = think
        return AsyncThrowingStream { continuation in
            let task = Task {
                // FAILS LOUD, INSIDE THE TURN — python's wording, verbatim.
                if model.isEmpty || model == DEFAULT_MODEL {
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
