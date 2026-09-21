// `llama-server` (llama.cpp) LLM backend.
//
// Mirrors python's `llamacpp_backend`. llama-server speaks the
// OpenAI-compatible surface at `<endpoint>/chat/completions` and streams RAW
// TEXT via SSE, so the wire decode is the SHARED
// `FantasticAICore.decodeOpenAISSELine` — the same one NIM uses.
//
// NO NATIVE TOOL-CALLING: the provider streams text and FantasticAICore parses
// the `<call name=…>` envelope out of it.
//
// Two things this backend does NOT pretend to own:
//
//   * THE MODEL. llama-server serves exactly one (whichever `-hf`/`-m` loaded)
//     and ignores the request's `model` field, so nothing is sent unless the
//     record names one.
//   * THE CONTEXT WINDOW. Fixed at server start (`-c N`); there is no
//     per-request equivalent, so set it on the llama-server command line.

import AsyncHTTPClient
import FantasticAICore
import FantasticJSON
import FantasticKernel
import Foundation
import NIOCore
import OrderedCollections

public let HANDLER_MODULE = "llamacpp_backend.tools"

/// Where a local llama-server listens by default.
public let DEFAULT_ENDPOINT = "http://127.0.0.1:8080/v1"

/// What `reflect.model` says when the record names none: the server loaded it.
/// (py-canonical `llamacpp_backend.provider.DEFAULT_MODEL`.)
public let DEFAULT_MODEL = "(server-loaded)"

public final class LlamaCppBundle: AgentBundle, @unchecked Sendable {
    public let name = "llamacpp_backend"
    public var verbs: [String] { AIBackend.verbNames }

    let core: AIBackend

    public init() {
        self.core = buildAIBackend(
            AIBackendConfig(
                kind: "llamacpp_backend",
                sentence:
                    "llama.cpp-backed local LLM agent (raw prompt-and-parse tool-calling).",
                // THE SERVER OWNS THE MODEL: llama-server serves exactly one,
                // named at ITS start, so reflect says so instead of guessing.
                defaultModel: DEFAULT_MODEL,
                defaultEndpoint: DEFAULT_ENDPOINT,
                // The sampling knobs, python's order. `num_ctx` is deliberately
                // ABSENT — llama-server fixes the window at start (`-c N`) and
                // this agent does not claim to own one.
                reflectExtra: { agent in
                    [
                        "temperature": .double(
                            agent.metaValue(forKey: "temperature")?.asDouble ?? 0.3),
                        "num_predict": Self.posInt(agent, "num_predict").map(JSON.integer) ?? .null,
                        "think": agent.metaValue(forKey: "think")?.asBool.map(JSON.bool) ?? .null,
                        "has_api_key": .bool(
                            pyTruthy(agent.metaValue(forKey: "api_key") ?? .null)),
                    ]
                },
                makeProvider: { agent, _, _ in
                    .provider(
                        LlamaCppProvider(
                            endpoint: Self.endpoint(agent: agent),
                            requestedModel: agent.metaValue(forKey: "model")?.asString,
                            apiKey: agent.metaValue(forKey: "api_key")?.asString,
                            maxTokens: agent.metaValue(forKey: "num_predict")?.asInt,
                            // `0.3 if temperature is None else float(temperature)`
                            // — the DEFAULT lives here, and the field is always
                            // sent (see the body below).
                            temperature: agent.metaValue(forKey: "temperature")?.asDouble ?? 0.3,
                            think: agent.metaValue(forKey: "think")?.asBool))
                }
            ))
    }

    public var readme: String? { Self.readmeText }

    /// THE PYTHON FILE, BYTE FOR BYTE (`llamacpp_backend/readme.md`). Persisted as
    /// `agents/<id>/readme.md` and served by `reflect readme=true`, so a swift
    /// kernel and a python one hand a reader the same document. A swift-written
    /// summary of it was a second, shorter truth about the same bundle.
    static let readmeText = #"""
        # llamacpp_backend — local LLM via `llama-server`, a context-managed completion agent

        Talks to a local `llama-server` (llama.cpp) over its OpenAI-compatible surface.
        Per-client chat threads, FIFO lock, and automatic context management (compaction /
        recall / context_status). Persistence via `file_bridge_id`. **No agentic loop** —
        one provider pass per `send`.

        ## Implementation
        A thin binding over the shared `ai_core` lib. The queue / FIFO lock, prompt
        assembly, the ONE-PASS `_run`, and all the context machinery live in
        `ai_core.core`; this module supplies the `LlamaCppProvider` builder (endpoint /
        model / sampling knobs from the agent record) and `ai_core.build()` wires the rest
        into `(VERBS, handler)`.

        Separate from `nvidia_nim_backend` even though both speak the OpenAI wire: a
        `reflect` describing a local model as an NVIDIA NIM agent would put a lie in the
        tree, and this system's whole premise is that the tree describes itself truthfully.

        ## Why not the ollama backend
        `llama-server` mounts the OpenAI surface and nothing else — `/api/chat`,
        `/api/tags`, `/api/generate` and `/api/show` all return 404, and the ollama
        backend's provider builds on the `ollama` SDK, which posts to `/api/chat`. Pointing
        its `endpoint` at a llama-server just posts into a 404.

        ## The thinking channel — the reason this bundle exists
        A reasoning model splits its output. Started with `--reasoning-format deepseek`,
        `llama-server` streams the thought channel as `reasoning_content` and only the
        conclusion as `content`. Reading `content` alone silently drops whole turns: the
        model spends a pass reasoning, you observe an EMPTY completion, and the chain dies
        with no error anywhere. This provider reads BOTH and wraps the thought channel once
        in `<think>…</think>` — parsed for tool calls, stripped before the turn is
        persisted (`ai_core.core.without_thinking`).

        `gemma-4` additionally GATES that channel behind a template kwarg
        `enable_thinking`, **default false**, and emits `<|channel>thought … <channel|>`
        rather than `<think>` tags. Set `think` on the record and this backend sends
        `chat_template_kwargs: {"enable_thinking": …}` per request — so toggling thinking
        is a record edit, not a server restart.

        ## Server side — the operator's job, not this agent's
        Nothing here starts or downloads anything. Run the server consciously:

        ```sh
        LLAMA_CACHE="$HOME/Projects/models" llama-server \
          -hf <org>/<repo>:<quant> --host 127.0.0.1 --port 8080 \
          -c 16384 -ngl 99 --jinja \
          --chat-template-kwargs '{"enable_thinking":true}' \
          --reasoning-format deepseek
        ```

        `--reasoning-format` decides where thoughts land: `deepseek` →
        `message.reasoning_content` (what this provider reads); `deepseek-legacy` → both
        inline and in the field; `none` → unparsed inline in `content`.

        ## Record fields
        - `endpoint` — default `http://127.0.0.1:8080/v1` (llama-server's own bind + path).
        - `model` — usually omit it. `llama-server` serves exactly ONE model, whichever
          `-hf`/`-m` loaded, and ignores the request's `model` field; when unset the key is
          not sent at all. Set it only when a proxy in front of the server routes on it.
        - `api_key` — llama-server is KEYLESS unless started with `--api-key`. Absent ⇒ no
          `Authorization` header is sent; none is invented.
        - `temperature` — default 0.3 (an agentic backend must emit exact tool-call JSON,
          not improvise).
        - `num_predict` — generation cap (`max_tokens`). Unset ⇒ the server's default.
        - `think` — `enable_thinking` per request. Unset ⇒ the kwarg is not sent (a model
          without a thinking gate rejects an unknown kwarg).

        There is **no `num_ctx`**: the context window is fixed at server start (`-c N`) and
        has no per-request equivalent, so this agent does not pretend to own one.

        ## Calling this agent
        `send {type:'send', text, client_id?}` runs ONE completion (per-backend FIFO lock)
        and returns `{response, final}`. A first-class peer of `exec_bridge`: a
        scheduler, a host job, a peer kernel, or a routine can call it by id. Optional
        `system_prompt:str` REPLACES the default system block. Optional `messages:list`
        REPLACES persisted history (fully stateless — no `file_bridge_id` needed, and no
        compaction — the caller owns the context in that mode).

        An in-flight `send` is a kernel TASK: kill it with the universal `ps` → `cancel`
        system verbs. There is no `interrupt` verb.

        """#

    public func handle(agentId: AgentId, payload: JSON, kernel: Kernel) async throws -> JSON? {
        try await core.handle(agentId: agentId, payload: payload, kernel: kernel)
    }

    public func onCancel(agentId: AgentId, taskId: TaskId, kernel: Kernel) async throws {
        await core.onCancel(agentId: agentId, taskId: taskId, kernel: kernel)
    }

    fileprivate static func endpoint(agent: Agent) -> String {
        agent.metaValue(forKey: "endpoint")?.asString ?? DEFAULT_ENDPOINT
    }

    /// `int(rec[k]) if rec.get(k) else None` — a falsy value (0, "", null) is
    /// NOT a knob, exactly as python's reflect_extra reads it.
    fileprivate static func posInt(_ agent: Agent, _ k: String) -> Int64? {
        guard let v = agent.metaValue(forKey: k), pyTruthy(v), let i = try? pyInt(v) else {
            return nil
        }
        return Int64(i)
    }
}

/// llama-server `/chat/completions` SSE provider. PURE RAW TEXT: yields
/// `.token`s decoded from BOTH channels through the shared OpenAI decode.
struct LlamaCppProvider: AIProvider {
    let endpoint: String
    /// What the RECORD asked for, if anything. llama-server serves exactly one
    /// model and ignores this field, so it is usually nil and nothing is sent.
    let requestedModel: String?

    /// The protocol wants a model id. Reporting a guess here would be a lie the
    /// reflect surface then repeats, so an unset one says whose choice it is.
    var model: String { requestedModel ?? "(llama-server default)" }
    let apiKey: String?
    let maxTokens: Int64?
    /// ALWAYS SENT. python's provider defaults it to 0.3 at construction and
    /// writes `if self._temperature is not None:` — which is never false. Swift
    /// carried an optional straight from the record, so an agent with no
    /// `temperature` meta ran at llama-server's own default (much higher) while
    /// its own `reflect` reported 0.3.
    let temperature: Double
    /// `nil` means DO NOT SEND the kwarg — models without a thinking gate
    /// reject an unknown one, so this is three-valued on purpose.
    let think: Bool?

    func chat(messages: [JSON]) -> AsyncThrowingStream<AIChunk, Error> {
        let endpoint = endpoint
        let requestedModel = requestedModel
        let apiKey = apiKey
        let maxTokens = maxTokens
        let temperature = temperature
        let think = think
        return AsyncThrowingStream { continuation in
            let task = Task {
                var body: OrderedDictionary<String, JSON> = [:]
                body["messages"] = .array(messages)
                body["stream"] = .bool(true)
                if let requestedModel { body["model"] = .string(requestedModel) }
                body["temperature"] = .double(temperature)
                if let maxTokens { body["max_tokens"] = .integer(maxTokens) }
                if let think {
                    body["chat_template_kwargs"] = .object([
                        "enable_thinking": .bool(think)
                    ])
                }

                // `endpoint.rstrip("/")` — EVERY trailing slash, not one.
                var base = endpoint
                while base.hasSuffix("/") { base.removeLast() }
                var req = HTTPClientRequest(url: "\(base)/chat/completions")
                req.method = .POST
                req.headers.add(name: "Content-Type", value: "application/json")
                req.headers.add(name: "Accept", value: "text/event-stream")
                if let apiKey, !apiKey.isEmpty {
                    req.headers.add(name: "Authorization", value: "Bearer \(apiKey)")
                }
                req.body = .bytes(ByteBuffer(string: JSON.object(body).serialize()))

                do {
                    let response = try await HTTPClient.shared.execute(req, timeout: .seconds(600))
                    let code = Int(response.status.code)
                    if code >= 400 {
                        continuation.finish(throwing: LlamaCppError.http(code))
                        return
                    }
                    // ONE wrapper for the whole turn — the reasoning markers open
                    // and close once, not per chunk.
                    var wrap = ThinkingWrap()
                    for try await line in bytesToLines(response.body) {
                        let decoded = decodeOpenAISSELine(line, wrap: &wrap)
                        for token in decoded.tokens { continuation.yield(.token(token)) }
                        if decoded.done { break }
                    }
                    // The model spent the tail of its turn reasoning: close the
                    // span so the text a parser reads stays balanced.
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

enum LlamaCppError: Error, CustomStringConvertible {
    case http(Int)
    var description: String {
        switch self {
        case .http(let code): return "HTTP \(code)"
        }
    }
}
