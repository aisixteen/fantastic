// vLLM LLM backend — talks to a vLLM server DIRECTLY.
//
// Mirrors python's `vllm_backend` (canonical) and rust's
// `fantastic-vllm-backend`. POSTs `{endpoint}/chat/completions` on vLLM's own
// OpenAI-compatible surface, with no proxy and no protocol translation in
// between, and streams RAW TEXT: FantasticAICore parses the `<call name=…>`
// envelope out of it. NO native tool-calling.
//
// What is vLLM-specific here, and why each piece exists (measured by the
// `dataroom` deployment on Qwen3.8-27B-FP8, vLLM 0.27.1, 2026-09-29):
//
//   * THE THOUGHT CHANNEL. With `--reasoning-parser qwen3` the thought streams
//     as `delta.reasoning_content` (some versions: `delta.reasoning`) and only
//     the answer as `delta.content`. Reading `content` alone drops whole turns.
//   * A DEAD TURN — all thinking, no answer — is the worst outcome in an agent
//     loop. `thinking_token_budget` is vLLM's hard stop for it: once spent,
//     vLLM injects the end-of-reasoning token and the model moves on to its
//     answer. It must stay BELOW `max_tokens` or it never fires.
//   * ONE system message, at index 0 — Qwen's chat template raises "System
//     message must be at the beginning" otherwise.
//   * A stream that ends with NO `finish_reason` means vLLM died mid-answer:
//     an error, never a completion.
//   * The KV-cache SESSION TABLE (`VllmSessionTable`): vLLM's prefix cache
//     makes a warm conversation answer ~6.5× faster; past the server's KV
//     capacity it preempts, and a preempted conversation re-prefills from
//     scratch. The table keeps the conversations THIS kernel drives under that
//     ceiling and rotates the least-recently-used one out.
//   * THE KEY, when the door needs one (owner, 2026-09-30: an HTTPS door in
//     front of the shared GPU takes `Authorization: Bearer <key>`). Read before
//     EACH request through `key` — the record's `credentials` ref in a credential
//     store (`Credentials.swift`), never the environment or the record; a record
//     with no `credentials` sends no header at all, so a local vLLM started
//     without `--api-key` keeps working. A 401 or 403 says to name `credentials`.

import AsyncHTTPClient
import CryptoKit
import FantasticAICore
import FantasticJSON
import FantasticKernel
import Foundation
import NIOCore
import OrderedCollections

public let HANDLER_MODULE = "vllm_backend.tools"

let NAME = "vllm_backend"

// MARK: - sampling

/// Applied ONLY where the record sets nothing (owner's decision: the dataroom
/// deployment's agentic defaults, copied). Python `SAMPLING_DEFAULTS`, in its
/// order — reflect shows them in this order and the body sends them in it.
public let SAMPLING_DEFAULTS: OrderedDictionary<String, JSON> = [
    "temperature": .double(0.2),
    "top_p": .double(0.8),
    "top_k": .integer(20),
    // A CHAT-TEMPLATE input (a soft steer on how much the model wants to
    // think); measured at max_tokens 6000: xhigh 26,440 thinking chars / 0
    // answer, medium 5,607 / 18,935, low 4,313 / 20,199.
    "reasoning_effort": .string("medium"),
    // The HARD limit (see the header); medium thinks ~900 tokens on a normal
    // agent turn, so this only binds on a runaway.
    "thinking_token_budget": .integer(16000),
]

/// Below this, NO thinking budget is sent: a handful of tokens of thinking is
/// no thinking, and a model starved of it stops with an empty answer (measured:
/// an 8-token budget on a 16-token turn, 2026-09-30).
public let BUDGET_FLOOR = 256

/// vLLM accepts exactly these; "high" answers 400, so it is refused at write.
public let REASONING_EFFORTS = ["low", "medium", "xhigh"]

/// Sent only when set on the record — never defaulted. Penalties especially:
/// presence_penalty 0.7 caused looping and frequency_penalty collapsed output
/// into sub-word garbage at ~31k chars; both 0 stayed clean to ~87k chars.
public let PASSTHROUGH = [
    "min_p", "presence_penalty", "frequency_penalty", "repetition_penalty",
    "no_repeat_ngram_size", "seed", "stop",
]

// MARK: - connection

/// TWO timeouts on purpose. A stopped instance drops SYNs silently, so one long
/// timeout would hang a caller for the whole read deadline; connect fails fast.
/// Read is long because a max-length generation decodes for many minutes (a
/// clean run reached ~24k output tokens). Both are the dataroom's values; each
/// is a record field.
public let CONNECT_TIMEOUT = 5.0
public let READ_TIMEOUT = 900.0

// MARK: - the KV-cache session table

/// Seconds a conversation may sit idle before its slot frees itself (dataroom's
/// value; a record field).
public let KV_TTL = 1800.0
public let KV_OVERFLOWS = ["evict", "reject"]

/// The LIVE conversations on ONE vLLM endpoint, least-recently-used first.
///
/// Shared by every vllm_backend agent in this kernel that names the same
/// endpoint (owner's decision: one table per endpoint, the closest a kernel
/// gets to "one count per GPU"). It is BOOKKEEPING, not memory management:
/// vLLM owns its prefix cache (content-addressed blocks, its own LRU) and the
/// table frees no GPU memory — a conversation rotated out here that comes back
/// still hits whatever blocks vLLM kept. What the table does is keep the number
/// of conversations this kernel drives at once under the server's KV ceiling,
/// so vLLM never has to preempt one of ours. It sees only THIS kernel's
/// traffic, so it is a local policy under the real ceiling.
///
/// Each admission uses the ADMITTING agent's own `kv_slots`/`kv_overflow`/
/// `kv_ttl` against the shared live set. Python `SessionTable`.
final class VllmSessionTable: @unchecked Sendable {
    private let lock = NSLock()
    /// key → last use, least-recently used FIRST (python's `OrderedDict`).
    private var live: OrderedDictionary<String, Double> = [:]
    private var evicted = 0
    private var rejected = 0

    /// Mark `key` live and most-recently used. Returns the key ROTATED OUT to
    /// make room (`evict`), or nil; throws `KvSlotsFull` (`reject`).
    func admit(_ key: String, slots: Int, overflow: String, ttl: Double, now: Double)
        throws(PyError) -> String?
    {
        // Not `withLock`: it rethrows untyped, and the refusal is a typed PyError.
        lock.lock()
        defer { lock.unlock() }
        // idle past its TTL: the slot frees itself
        live = live.filter { now - $0.value <= ttl }
        if live[key] != nil {
            // re-inserted, so it moves to the most-recently-used end
            live.removeValue(forKey: key)
            live[key] = now
            return nil
        }
        var rotated: String? = nil
        if live.count >= slots {
            if overflow == "reject" {
                rejected += 1
                // the soonest a slot frees on its own: the idlest one's TTL
                let retryAfter = max(0.0, ttl - (now - (live.values.first ?? now)))
                throw PyError(
                    "KvSlotsFull",
                    "vllm_backend: all \(slots) KV slots on this endpoint are live "
                        + "(kv_overflow=reject) — retry in ~\(String(format: "%.0f", retryAfter))s, "
                        + "or set kv_overflow=evict to rotate the least-recently-used out")
            }
            rotated = live.keys.first
            live.removeFirst()
            evicted += 1
        }
        live[key] = now
        return rotated
    }

    /// What reflect shows under `kv` — python `SessionTable.view`, its key order.
    func view() -> OrderedDictionary<String, JSON> {
        lock.withLock {
            [
                "live": .integer(Int64(live.count)),
                // least-recently used first
                "conversations": .array(live.keys.map { .string($0) }),
                "evicted": .integer(Int64(evicted)),
                "rejected": .integer(Int64(rejected)),
            ]
        }
    }
}

/// endpoint (normalised) → its table, PER KERNEL. Python's table is one per
/// endpoint in its one kernel ("it sees only THIS kernel's traffic"); swift
/// hosts many kernels in one process, so the map is keyed by `kernel.serial`
/// like the AI core's side tables (`AIRunStates`), and a kernel's teardown takes
/// its tables with it (`VllmSideTables`).
enum VllmTables {
    private static let lock = NSLock()
    nonisolated(unsafe) private static var tables: [UInt64: [String: VllmSessionTable]] = [:]

    static func table(_ kernelSerial: UInt64, for endpoint: String) -> VllmSessionTable {
        let key = rstripSlashes(endpoint)
        return lock.withLock {
            if let t = tables[kernelSerial]?[key] { return t }
            let t = VllmSessionTable()
            tables[kernelSerial, default: [:]][key] = t
            return t
        }
    }

    /// Drop a dead kernel's whole sub-table.
    static func forget(_ kernel: Kernel) {
        lock.withLock { _ = tables.removeValue(forKey: kernel.serial) }
    }
}

/// `endpoint.rstrip("/")` — EVERY trailing slash, not one.
func rstripSlashes(_ s: String) -> String {
    var out = s
    while out.hasSuffix("/") { out.removeLast() }
    return out
}

/// The thinking budget each agent's last pass actually SENT — surfaced in
/// reflect, so a caller debugging a cut can see what the adapted default
/// became. The provider is built per send, so it lives here, keyed by kernel
/// (`kernel.serial`) and agent id, cleared on delete and on the kernel's
/// teardown.
enum VllmLastBudget {
    private static let lock = NSLock()
    nonisolated(unsafe) private static var values: [UInt64: [String: Int]] = [:]

    static func set(_ kernelSerial: UInt64, _ agent: String, _ budget: Int?) {
        lock.withLock { values[kernelSerial, default: [:]][agent] = budget }
    }

    static func get(_ kernelSerial: UInt64, _ agent: String) -> Int? {
        lock.withLock { values[kernelSerial]?[agent] }
    }

    static func clear(_ kernelSerial: UInt64, _ agent: String) {
        lock.withLock { _ = values[kernelSerial]?.removeValue(forKey: agent) }
    }

    /// Drop a dead kernel's whole sub-table.
    static func forget(_ kernel: Kernel) {
        lock.withLock { _ = values.removeValue(forKey: kernel.serial) }
    }
}

/// The vllm side tables are keyed by `Kernel.serial`, so a kernel that shuts
/// down must take its rows with it. Registered ONCE, when the bundle is first
/// built — the kernel cannot reach into this module itself.
enum VllmSideTables {
    private static let lock = NSLock()
    nonisolated(unsafe) private static var installed = false

    static func installTeardownHook() {
        lock.withLock {
            if installed { return }
            installed = true
            Kernel.onTeardown { kernel in
                VllmTables.forget(kernel)
                VllmLastBudget.forget(kernel)
            }
        }
    }
}

// MARK: - messages

/// `str(m.get("content", ""))` — a MISSING key is "", an explicit null is
/// python's "None" (`pyStr`), exactly as python reads it.
private func contentText(_ m: JSON) -> String {
    guard let obj = m.asObject, let v = obj["content"] else { return "" }
    return pyStr(v)
}

private func role(_ m: JSON) -> String? { m.asObject?["role"]?.asString }

/// A conversation's identity: its system text + first user turn.
///
/// That is the PREFIX vLLM caches, and it is stable across turns because the
/// history is append-only. When compaction rewrites it the key changes — and
/// so has the prefix, so the old cache no longer applies: counting it as a new
/// conversation is the truth, and the stale one ages out of the table.
func conversationKey(_ messages: [JSON]) -> String {
    let system = messages.filter { role($0) == "system" }.map(contentText).joined(
        separator: "\n\n")
    let firstUser = messages.first { role($0) == "user" }.map(contentText) ?? ""
    let digest = SHA256.hash(data: Data("\(system)\u{0}\(firstUser)".utf8))
    return String(digest.map { String(format: "%02x", $0) }.joined().prefix(16))
}

/// Every system message merged into ONE at index 0 (Qwen's template refuses a
/// system message anywhere else); the other turns keep their order.
func oneSystemMessage(_ messages: [JSON]) -> [JSON] {
    let systems = messages.filter { role($0) == "system" }.map(contentText)
    let rest = messages.filter { role($0) != "system" }
    if systems.isEmpty { return rest }
    let merged: JSON = .object([
        "role": .string("system"), "content": .string(systems.joined(separator: "\n\n")),
    ])
    return [merged] + rest
}

// MARK: - failures, in words

/// Where a transport failure happened: before a response head (connecting), or
/// while its body streamed.
enum VllmPhase { case connect, stream }

/// A transport failure, said as what it MEANS for this server — python maps
/// httpx's exception classes to the same four sentences. Cancellation passes
/// through untouched (a cancelled turn is not a failed one).
func vllmTransportError(
    _ error: Error, endpoint: String, connectTimeout: Double, readTimeout: Double,
    phase: VllmPhase
) -> Error {
    if error is CancellationError || error is PyError { return error }
    if let e = error as? HTTPClientError {
        if e == .connectTimeout {
            return PyError(
                "RuntimeError",
                "vllm_backend: no answer from \(endpoint) within "
                    + "connect_timeout=\(String(format: "%g", connectTimeout))s — the instance is "
                    + "STOPPED (its SYNs are dropped), or the tunnel/port is wrong")
        }
        if e == .readTimeout {
            return PyError(
                "RuntimeError",
                "vllm_backend: \(endpoint) sent nothing for "
                    + "read_timeout=\(String(format: "%g", readTimeout))s")
        }
        if e == .cancelled { return CancellationError() }
        // python `httpx.RemoteProtocolError`: the peer hung up without an
        // answer — the stream sentence, whichever phase it hit.
        if e != .remoteConnectionClosed, phase == .connect {
            // A REQUEST the client refuses before any connection (no scheme, a
            // bad URL, an empty host) is not a refused connection: python's
            // httpx raises `UnsupportedProtocol`/`InvalidURL` with its own text
            // and the turn fails on it, unmapped.
            return error
        }
    }
    switch phase {
    case .connect where !(error is HTTPClientError):
        // No response head ever arrived: the connection itself failed —
        // python's `httpx.ConnectError` (a refused port, a tunnel with nothing
        // behind it).
        return PyError(
            "RuntimeError",
            "vllm_backend: \(endpoint) refused the connection — the host is up but vLLM is "
                + "not listening yet (it loads for ~75 s after boot), or the tunnel in front of "
                + "it is down")
    case .connect, .stream:
        // The head arrived and the body broke off (or the peer closed with no
        // head at all) — python's
        // `httpx.ReadError` / `RemoteProtocolError`.
        return PyError(
            "RuntimeError",
            "vllm_backend: \(endpoint) dropped the stream mid-answer — vLLM died or "
                + "restarted; this turn is NOT complete")
    }
}

// MARK: - the HTTP clients

/// One client per (connect, read) timeout pair, for the life of the process —
/// the timeouts are CLIENT configuration in AsyncHTTPClient, and agents on the
/// same endpoint usually share them.
enum VllmHTTP {
    private static let lock = NSLock()
    nonisolated(unsafe) private static var clients: [String: HTTPClient] = [:]

    static func client(connect: Double, read: Double) -> HTTPClient {
        let key = "\(connect)|\(read)"
        return lock.withLock {
            if let c = clients[key] { return c }
            // A refused connect is retried by the client's pool until the
            // connect timeout, then reported as the refusal itself (measured:
            // POSIX 61 after 5.0 s) — python's httpx reports it at once. The
            // sentence is the same; the short connect timeout bounds the wait.
            let c = HTTPClient(
                eventLoopGroupProvider: .singleton,
                configuration: .init(
                    timeout: .init(
                        connect: .nanoseconds(Int64(connect * 1_000_000_000)),
                        read: .nanoseconds(Int64(read * 1_000_000_000)))))
            clients[key] = c
            return c
        }
    }
}

// MARK: - the provider

/// vLLM `/chat/completions` SSE provider. PURE RAW TEXT: yields `.token`s from
/// both channels, the thought wrapped once in `<think>…</think>`.
struct VllmProvider: AIProvider {
    /// `endpoint.rstrip("/")`.
    let endpoint: String
    let model: String
    /// `key(endpoint) -> key?`, awaited before EACH request; nil = no
    /// `credentials` on the record, no header.
    let key: KeyReader
    /// The record's output budget — bounds a turn; a side call brings its own.
    let maxTokens: Int
    /// The record's `context_window`. A turn whose `max_tokens` reaches it asks for
    /// "the rest of the window": vLLM counts the prompt AND the output against one
    /// window, so a cap that is the whole window is refused (HTTP 400). Such a turn
    /// sends NO max_tokens — vLLM then gives it what the prompt leaves (owner,
    /// 2026-10-04: the e2e limit is the whole 260k). python `VllmProvider._window`.
    var window: Int? = nil
    let kvSlots: Int
    let kvOverflow: String
    let kvTtl: Double
    /// The effective sampling: the record's values over SAMPLING_DEFAULTS, plus
    /// any PASSTHROUGH field the record set.
    let sampling: OrderedDictionary<String, JSON>
    /// Whether `thinking_token_budget` came from the RECORD (the caller's
    /// statement — never touched) or from the default (ours to fit per call).
    let explicitBudget: Bool
    let connectTimeout: Double
    let readTimeout: Double
    /// Whose last budget reflect reports (the agent id), and the kernel whose
    /// tables this provider reads and writes (`kernel.serial`).
    let agentKey: String
    let kernelSerial: UInt64
    /// Seconds on a monotonic clock — the table's TTL arithmetic.
    let clock: @Sendable () -> Double
    /// What the endpoint REPORTED for the last pass — the prompt size (ai_core
    /// calibrates with it) and the stop reason.
    let usage: AIUsage? = AIUsage()

    /// The `thinking_token_budget` to send on a call bounded by `callMax`
    /// (owner's rule, 2026-09-30; python `VllmProvider._budget_for`):
    ///
    /// - a MAIN turn with an EXPLICIT budget sends it as set — the write keeps
    ///   it below the record's `max_tokens`, so it can fire;
    /// - otherwise (the DEFAULT on a main turn, or any budget on a bounded
    ///   SIDE CALL such as a fold summary) it is `min(budget, callMax // 2)`, so
    ///   at least half of the call's output is left for the answer (a rule of
    ///   thumb, not tuned) — an explicit 8000 on a ~2000-token fold summary
    ///   becomes 1000 instead of refusing the fold;
    /// - below `BUDGET_FLOOR` NO budget is sent at all: a few tokens of thinking
    ///   is no thinking — measured, an 8-token budget made the model stop with
    ///   thinking only and no answer (a dead turn).
    func budget(for callMax: Int, sideCall: Bool) throws(PyError) -> Int? {
        guard let raw = sampling["thinking_token_budget"], raw != .null else { return nil }
        let budget = try pyInt(raw)
        if explicitBudget && !sideCall { return budget }
        // python's `//`: floor division, also for a negative bound
        let half = Int((Double(callMax) / 2).rounded(.down))
        let adapted = min(budget, half)
        return adapted < BUDGET_FLOOR ? nil : adapted
    }

    /// The request body in PYTHON'S KEY ORDER — model, messages, max_tokens,
    /// stream, stream_options, the sampling keys, then the budget.
    func body(_ messages: [JSON], callMax: Int, sideCall: Bool) throws(PyError)
        -> OrderedDictionary<String, JSON>
    {
        var body: OrderedDictionary<String, JSON> = [
            "model": .string(model),
            "messages": .array(oneSystemMessage(messages)),
            "max_tokens": .integer(Int64(callMax)),
            "stream": .bool(true),
            // the final chunk carries usage (and an EMPTY choices array)
            "stream_options": .object(["include_usage": .bool(true)]),
        ]
        if !sideCall, let w = window, callMax >= w {
            body.removeValue(forKey: "max_tokens")  // the rest of the window — see `window`
        }
        for key in ["temperature", "top_p", "top_k", "reasoning_effort"] + PASSTHROUGH {
            if let v = sampling[key], v != .null { body[key] = v }
        }
        let budget = try budget(for: callMax, sideCall: sideCall)
        VllmLastBudget.set(kernelSerial, agentKey, budget)
        if let budget { body["thinking_token_budget"] = .integer(Int64(budget)) }
        return body
    }

    func chat(messages: [JSON]) -> AsyncThrowingStream<AIChunk, Error> {
        chat(messages: messages, maxTokens: nil)
    }

    /// `maxTokens` bounds a side call (a fold summary); nil, the record's
    /// `max_tokens` bounds the turn.
    func chat(messages: [JSON], maxTokens sideBound: Int?) -> AsyncThrowingStream<AIChunk, Error> {
        let provider = self
        let callMax = sideBound ?? maxTokens
        return AsyncThrowingStream { continuation in
            let task = Task {
                do {
                    try await provider.run(
                        messages, callMax: callMax, sideBound: sideBound, continuation)
                    continuation.finish()
                } catch {
                    continuation.finish(throwing: error)
                }
            }
            continuation.onTermination = { _ in task.cancel() }
        }
    }

    private func run(
        _ messages: [JSON], callMax: Int, sideBound: Int?,
        _ continuation: AsyncThrowingStream<AIChunk, Error>.Continuation
    ) async throws {
        let payload = JSON.object(
            try body(messages, callMax: callMax, sideCall: sideBound != nil)
        ).serialize()
        // Admission BEFORE the request: a `reject` must not reach the GPU.
        _ = try VllmTables.table(kernelSerial, for: endpoint).admit(
            conversationKey(messages), slots: kvSlots, overflow: kvOverflow, ttl: kvTtl,
            now: clock())

        var req = HTTPClientRequest(url: "\(endpoint)/chat/completions")
        req.method = .POST
        req.headers.add(name: "Content-Type", value: "application/json")
        req.headers.add(name: "Accept", value: "text/event-stream")
        let apiKey = try await key(endpoint).flatMap { $0.isEmpty ? nil : $0 }
        if let apiKey {
            req.headers.add(name: "Authorization", value: "Bearer \(apiKey)")
        }
        req.body = .bytes(ByteBuffer(string: payload))

        let client = VllmHTTP.client(connect: connectTimeout, read: readTimeout)
        let response: HTTPClientResponse
        do {
            response = try await client.execute(req, deadline: .distantFuture)
        } catch {
            throw vllmTransportError(
                error, endpoint: endpoint, connectTimeout: connectTimeout,
                readTimeout: readTimeout, phase: .connect)
        }
        let code = Int(response.status.code)
        if code >= 400 {
            let collected = try? await response.body.collect(upTo: 1 << 20)
            // python `.decode("utf-8", "replace")[:500]` — 500 CHARACTERS
            let detail = String((collected.map { String(buffer: $0) } ?? "").prefix(500))
            let why =
                code == 401 || code == 403
                ? " — the door wants a key: name it in `credentials` ({store, ref})" : ""
            throw PyError(
                "RuntimeError",
                "vllm_backend: \(endpoint) answered HTTP \(code)\(why): \(detail)")
        }

        var wrap = ThinkingWrap()
        var sawFinish = false
        do {
            for try await line in bytesToLines(response.body) {
                // `bytesToLines` reassembles lines a read split mid-JSON;
                // anything that is not a `data:` line (a blank separator, a `:`
                // keep-alive) carries nothing.
                guard line.hasPrefix("data:") else { continue }
                let data = String(line.dropFirst(5)).trimmingCharacters(
                    in: .whitespacesAndNewlines)
                if data == "[DONE]" { break }
                if data.isEmpty { continue }
                let obj: JSON
                do {
                    obj = try JSON.parse(data)
                } catch {
                    // python's `json.loads` raises; so does this
                    throw PyError("JSONDecodeError", "\(error)")
                }
                if let usage, case .object(let u)? = obj.asObject?["usage"],
                    let n = u["prompt_tokens"], pyTruthy(n), let tokens = n.asInt
                {
                    usage.lastPromptTokens = Int(tokens)
                }
                guard let choices = obj["choices"].asArray, !choices.isEmpty else {
                    continue  // the usage-only final chunk
                }
                let choice = choices[0]
                if pyTruthy(choice["finish_reason"]) {
                    sawFinish = true
                    let fr = pyStr(choice["finish_reason"])
                    // a `length` cut is surfaced as a cut, so the caller can
                    // continue — never read as a finished answer
                    usage?.lastStopReason = fr == "length" ? "max_tokens" : fr
                }
                let delta = choice["delta"]
                // python `d.get("reasoning_content") or d.get("reasoning")`
                var think = delta["reasoning_content"]
                if !pyTruthy(think) { think = delta["reasoning"] }
                if let t = think.asString, !t.isEmpty {
                    for token in wrap.reasoning(t) { continuation.yield(.token(token)) }
                }
                if let c = delta["content"].asString, !c.isEmpty {
                    for token in wrap.content(c) { continuation.yield(.token(token)) }
                }
            }
        } catch {
            throw vllmTransportError(
                error, endpoint: endpoint, connectTimeout: connectTimeout,
                readTimeout: readTimeout, phase: .stream)
        }
        // A turn that ended in thought still closes its span, so the text a
        // parser reads stays balanced.
        for token in wrap.finish() { continuation.yield(.token(token)) }
        if !sawFinish {
            throw PyError(
                "RuntimeError",
                "vllm_backend: the stream from \(endpoint) ended without a finish_reason — vLLM "
                    + "died mid-answer; this turn is NOT complete")
        }
        // A CUT: a BOUNDED SIDE CALL (a fold summary, which passed its own
        // `max_tokens`) that stopped at its bound got what it asked for — an
        // answer; its caller reads `lastStopReason`. A MAIN turn that hit the
        // record's `max_tokens` did not finish, and returned as if it had, it
        // parses as a truncated envelope (or, as measured on vLLM, a turn of pure
        // thinking) and the chain dies downstream with the cause hidden — so it
        // fails, naming the cut. Same contract as every other backend.
        if sideBound == nil && usage?.lastStopReason == "max_tokens" {
            throw PyError(
                "RuntimeError",
                "vllm_backend: the turn was cut at max_tokens=\(callMax) "
                    + "(finish_reason=length) — it did not finish; raise max_tokens on "
                    + "the record, or lower thinking_token_budget / reasoning_effort")
        }
    }
}

// MARK: - the bundle

public final class VllmBundle: AgentBundle, @unchecked Sendable {
    public let name = "vllm_backend"
    public var verbs: [String] { AIBackend.verbNames }

    let core: AIBackend

    public init() {
        VllmSideTables.installTeardownHook()
        self.core = buildAIBackend(
            AIBackendConfig(
                kind: NAME,
                sentence:
                    "vLLM-backed LLM agent, talking to the vLLM server directly "
                    + "(raw prompt-and-parse tool-calling; a KV-cache session table per endpoint).",
                reflectExtra: { agent, kernel in Self.reflectExtra(agent, kernel) },
                makeProvider: { agent, _, kernel in
                    try Self.makeProvider(
                        agent, kernel: kernel,
                        key: keyReader(agent.id, kernel, backend: NAME, required: false))
                }
            ))
    }

    public var readme: String? { Self.readmeText }

    public func handle(agentId: AgentId, payload: JSON, kernel: Kernel) async throws -> JSON? {
        try await core.handle(agentId: agentId, payload: payload, kernel: kernel)
    }

    public func onCancel(agentId: AgentId, taskId: TaskId, kernel: Kernel) async throws {
        try await core.onCancel(agentId: agentId, taskId: taskId, kernel: kernel)
    }

    /// The shared delete hook, plus this backend's own table: the last-budget
    /// entry is this agent's process state, and it goes with it too.
    public func onDelete(agentId: AgentId, kernel: Kernel) async throws {
        await core.onDelete(agentId: agentId, kernel: kernel)
        VllmLastBudget.clear(kernel.serial, agentId.value)
    }

    // MARK: record → provider

    /// The effective sampling — the record's values over the defaults — and
    /// whether `thinking_token_budget` was set on the RECORD (explicit: sent as
    /// is) or defaulted (ours to adapt per call). Python `_sampling`.
    static func sampling(_ agent: Agent) -> (OrderedDictionary<String, JSON>, Bool) {
        var eff: OrderedDictionary<String, JSON> = [:]
        for (k, dflt) in SAMPLING_DEFAULTS {
            if let v = agent.metaValue(forKey: k), v != .null { eff[k] = v } else { eff[k] = dflt }
        }
        for k in PASSTHROUGH {
            if let v = agent.metaValue(forKey: k), v != .null { eff[k] = v }
        }
        let explicit = agent.metaValue(forKey: "thinking_token_budget").map { $0 != .null } ?? false
        return (eff, explicit)
    }

    /// A count the record MUST name — never guessed, fails the turn loud.
    static func requiredInt(_ agent: Agent, _ key: String) throws(PyError) -> Int {
        guard case .integer(let n)? = agent.metaValue(forKey: key), n >= 1 else {
            throw PyError(
                "ValueError",
                "\(NAME): `\(key)` is required on the agent record, a positive int — "
                    + "nothing is guessed (update_agent <id> \(key)=…)")
        }
        return Int(n)
    }

    /// `float(rec.get(key) or default)` — an absent or falsy value is the
    /// default; anything else is python's `float()` of it.
    static func seconds(_ agent: Agent, _ key: String, _ dflt: Double) throws(PyError) -> Double {
        guard let v = agent.metaValue(forKey: key), pyTruthy(v) else { return dflt }
        return try pyFloat(v)
    }

    /// Build a VllmProvider from the agent record. `endpoint`, `model`,
    /// `max_tokens`, `kv_slots` and `kv_overflow` are REQUIRED — checked in
    /// python's order, so the FIRST missing one is the one named.
    static func makeProvider(
        _ agent: Agent,
        kernel: Kernel,
        key: @escaping KeyReader,
        clock: @escaping @Sendable () -> Double = {
            ProcessInfo.processInfo.systemUptime
        }
    ) throws(PyError) -> VllmProvider {
        let (sampling, explicit) = Self.sampling(agent)
        guard let overflow = agent.metaValue(forKey: "kv_overflow")?.asString,
            KV_OVERFLOWS.contains(overflow)
        else {
            throw PyError(
                "ValueError",
                "\(NAME): `kv_overflow` is required on the agent record — evict (the "
                    + "least-recently-used conversation rotates out) or reject (refuse the "
                    + "newcomer) (update_agent <id> kv_overflow=…)")
        }
        let endpoint = try requiredMeta(agent, "endpoint", NAME)
        let model = try requiredMeta(agent, "model", NAME)
        let maxTokens = try requiredInt(agent, "max_tokens")
        let kvSlots = try requiredInt(agent, "kv_slots")
        return VllmProvider(
            endpoint: rstripSlashes(endpoint),
            model: model,
            key: key,
            maxTokens: maxTokens,
            window: agent.metaValue(forKey: "context_window")?.asInt.map(Int.init).flatMap { $0 > 0 ? $0 : nil },
            kvSlots: kvSlots,
            kvOverflow: overflow,
            kvTtl: try seconds(agent, "kv_ttl", KV_TTL),
            sampling: sampling,
            explicitBudget: explicit,
            connectTimeout: try seconds(agent, "connect_timeout", CONNECT_TIMEOUT),
            readTimeout: try seconds(agent, "read_timeout", READ_TIMEOUT),
            agentKey: agent.id.value,
            kernelSerial: kernel.serial,
            clock: clock)
    }

    /// Merged into reflect: the EFFECTIVE sampling (defaults filled in), the
    /// timeouts, and the endpoint's shared KV session table — so one reflect
    /// shows what the next request will send and how many conversations are
    /// live. Python `_reflect_extra`, its key order.
    static func reflectExtra(_ agent: Agent, _ kernel: Kernel) -> OrderedDictionary<String, JSON> {
        let (sampling, explicit) = Self.sampling(agent)
        var kv: OrderedDictionary<String, JSON> = [
            "slots": agent.metaValue(forKey: "kv_slots") ?? .null,
            "overflow": agent.metaValue(forKey: "kv_overflow") ?? .null,
            "ttl": .double((try? seconds(agent, "kv_ttl", KV_TTL)) ?? KV_TTL),
        ]
        // shared by every vllm_backend agent on the same endpoint
        if case .string(let endpoint)? = agent.metaValue(forKey: "endpoint") {
            for (k, v) in VllmTables.table(kernel.serial, for: endpoint).view() { kv[k] = v }
        }
        return [
            "sampling": .object(sampling),
            "thinking_token_budget_source": .string(explicit ? "record" : "default"),
            // what the last pass actually SENT (the default adapts to small calls)
            "last_thinking_budget": VllmLastBudget.get(kernel.serial, agent.id.value).map {
                .integer(Int64($0))
            } ?? .null,
            "max_tokens": agent.metaValue(forKey: "max_tokens") ?? .null,
            "timeouts": .object([
                "connect": .double(
                    (try? seconds(agent, "connect_timeout", CONNECT_TIMEOUT)) ?? CONNECT_TIMEOUT),
                "read": .double(
                    (try? seconds(agent, "read_timeout", READ_TIMEOUT)) ?? READ_TIMEOUT),
            ]),
            "kv": .object(kv),
        ]
    }

    // MARK: validate_meta

    /// python `_positive`: an int (≥1) — or, for seconds, an int or float > 0.
    /// A bool is neither.
    static func positive(_ v: JSON?, integer: Bool) -> Bool {
        switch v {
        case .integer(let n)?: return integer ? n >= 1 : n > 0
        case .double(let d)?: return !integer && d > 0
        default: return false
        }
    }

    /// Refuse a write this backend cannot honour, AT THE WRITE, naming the legal
    /// set. Checks the MERGED state, because `thinking_token_budget` and
    /// `max_tokens` are only wrong together. Python `validate_meta`.
    public func validateMeta(
        _ meta: OrderedDictionary<String, JSON>, record: OrderedDictionary<String, JSON>
    ) -> String? {
        var merged = record
        for (k, v) in meta { merged[k] = v }
        for key in ["max_tokens", "kv_slots"] {
            if let v = meta[key], v != .null, !Self.positive(v, integer: true) {
                return "\(key) must be a positive int"
            }
        }
        for key in ["kv_ttl", "connect_timeout", "read_timeout"] {
            if let v = meta[key], v != .null, !Self.positive(v, integer: false) {
                return "\(key) must be a positive number of seconds"
            }
        }
        if let v = meta["kv_overflow"], v != .null, !KV_OVERFLOWS.contains(v.asString ?? "\u{0}") {
            return "kv_overflow=\(pyRepr(v)) is not a rotation policy — legal values: evict "
                + "(the least-recently-used conversation rotates out), reject (refuse the "
                + "newcomer)"
        }
        if let v = meta["reasoning_effort"], v != .null,
            !REASONING_EFFORTS.contains(v.asString ?? "\u{0}")
        {
            return "reasoning_effort=\(pyRepr(v)) is not one vLLM accepts — legal values: low, "
                + "medium, xhigh (\"high\" answers 400; xhigh needs a thinking_token_budget or "
                + "it can think without answering)"
        }
        let budget = merged["thinking_token_budget"]
        if meta["thinking_token_budget"] != nil, let budget, budget != .null,
            !Self.positive(budget, integer: true)
        {
            return "thinking_token_budget must be a positive int"
        }
        if case .integer(let b)? = budget, Self.positive(budget, integer: true),
            case .integer(let mx)? = merged["max_tokens"],
            Self.positive(merged["max_tokens"], integer: true),
            b >= mx
        {
            return "thinking_token_budget=\(b) is not below max_tokens=\(mx) — it could never "
                + "fire, and a turn would end mid-thought with no answer; keep it below "
                + "max_tokens (half is a good start)"
        }
        // EVERY required field, checked at the WRITE (owner's decision,
        // 2026-09-30): a model spawning a sub-AI copies the fields it can see,
        // and a record the write accepted but no turn could use failed only
        // later, on every turn, far from the create that caused it (measured:
        // E37 multisubai_build — the solvers carried endpoint/model/max_tokens
        // but no kv_slots/kv_overflow). Named ALL at once, from the schema, so
        // one retry fixes the record. Checked LAST, so a field that is present
        // but malformed gets its own, more specific refusal above.
        return RecordSchema.validate(Self.RECORD_SCHEMA, backend: NAME, meta: meta, record: record)
    }

    public var recordSchema: JSON? { Self.RECORD_SCHEMA }

    /// THE RECORD SCHEMA (lib request #3) — what this kind's record takes, read
    /// with no agent existing (`reflect bundles=all`). Built from the constants
    /// the provider runs on, and enforced by `validateMeta` above, so the page,
    /// the write check and the turn cannot disagree. The five required fields have
    /// NO default: nothing is guessed about a server. Python `RECORD_SCHEMA`.
    static let RECORD_SCHEMA: JSON = RecordSchema.schema(
        [
            RecordSchema.field("endpoint", "str", "the vLLM server's /v1 URL", required: true),
            RecordSchema.field("model", "str", "the served model name", required: true),
            RecordSchema.field(
                "max_tokens", "int", "a turn's output budget (tokens)", required: true),
            RecordSchema.field(
                "kv_slots", "int", "how many conversations may be live on the endpoint",
                required: true),
            RecordSchema.field(
                "kv_overflow", "enum",
                "all slots live: evict (the least-recently-used rotates out) or reject "
                    + "(the newcomer is refused)",
                required: true, enumValues: KV_OVERFLOWS),
            RecordSchema.field(
                "temperature", "float", "sampling temperature",
                default: SAMPLING_DEFAULTS["temperature"]),
            RecordSchema.field(
                "top_p", "float", "nucleus sampling", default: SAMPLING_DEFAULTS["top_p"]),
            RecordSchema.field(
                "top_k", "int", "top-k sampling", default: SAMPLING_DEFAULTS["top_k"]),
            RecordSchema.field(
                "reasoning_effort", "enum",
                "how much the model wants to think (xhigh needs a thinking_token_budget)",
                default: SAMPLING_DEFAULTS["reasoning_effort"], enumValues: REASONING_EFFORTS),
            RecordSchema.field(
                "thinking_token_budget", "int",
                "the hard cap on thinking tokens, below max_tokens; unset, a turn sends "
                    + "min(default, max_tokens/2)",
                default: SAMPLING_DEFAULTS["thinking_token_budget"]),
            RecordSchema.field("min_p", "float", "min-p sampling; sent only when set"),
            RecordSchema.field("presence_penalty", "float", "sent only when set"),
            RecordSchema.field("frequency_penalty", "float", "sent only when set"),
            RecordSchema.field("repetition_penalty", "float", "sent only when set"),
            RecordSchema.field("no_repeat_ngram_size", "int", "sent only when set"),
            RecordSchema.field("seed", "int", "sent only when set"),
            RecordSchema.field("stop", "list", "stop sequences; sent only when set"),
            RecordSchema.field(
                "kv_ttl", "float", "seconds an idle conversation keeps its slot",
                default: .double(KV_TTL)),
            RecordSchema.field(
                "connect_timeout", "float", "seconds to reach the server",
                default: .double(CONNECT_TIMEOUT)),
            RecordSchema.field(
                "read_timeout", "float", "seconds to wait on a generation",
                default: .double(READ_TIMEOUT)),
            // the door's key, when it wants one — absent = no Authorization header
            RecordSchema.credentialsField(required: false),
        ])

    /// The fields a turn cannot run without — refused at the write when absent.
    /// The schema's required fields, not a second list. Python `REQUIRED`.
    static let REQUIRED: [String] = (RECORD_SCHEMA["fields"].asArray ?? [])
        .filter { $0["required"].asBool == true }.compactMap { $0["name"].asString }
}

extension VllmBundle {
    /// THE PYTHON FILE, BYTE FOR BYTE (`vllm_backend/readme.md`). Persisted as
    /// `agents/<id>/readme.md` and served by `reflect readme=true`, so a swift
    /// kernel and a python one hand a reader the same document.
    static let readmeText = #"""
        # vllm_backend — an LLM agent talking to a vLLM server directly

        Talks to a vLLM server over its OpenAI-compatible chat surface
        (`POST {endpoint}/chat/completions`) with no proxy in between. Per-client chat
        threads, FIFO lock, and automatic context management (compaction / recall /
        context_status). Conversation in its memory store (`memory_store_id`). **No agentic
        loop** — one provider pass per `send`. Tool calls are this system's own `<call>`
        text: run the server WITHOUT a tool parser (one swallows the text and the answer
        comes back empty).

        ## The record

        REQUIRED — nothing is guessed, and a record missing any of them is refused at the
        write, naming every missing field (the whole field list, with defaults and hints, is
        this kind's `schema` in `reflect bundles=all`; `memory_store_id` is required too, as
        on every AI agent):
        - `endpoint` — the server's `/v1` base, e.g. `http://127.0.0.1:18000/v1`
        - `model` — the served model name, e.g. `qwen3.8-27b`
        - `max_tokens` — the output budget of a turn (unbounded, a runaway turn decodes for a long time)
        - `kv_slots` — how many conversations may be LIVE on this endpoint at once
        - `kv_overflow` — what happens to the next one: `evict` (the least-recently-used
          conversation rotates out) or `reject` (the newcomer is refused with a retry time)

        Defaults, used only where the record sets nothing: `temperature` 0.2, `top_p` 0.8,
        `top_k` 20, `reasoning_effort` medium (`low`|`medium`|`xhigh`), `thinking_token_budget`
        16000, `kv_ttl` 1800 s, `connect_timeout` 5 s, `read_timeout` 900 s. Sent only when set:
        `min_p`, `presence_penalty`, `frequency_penalty`, `repetition_penalty`,
        `no_repeat_ngram_size`, `seed`, `stop`. Mind the server: under speculative decoding
        vLLM answers `min_p` with a 400, and presence/frequency penalties measured worse
        (looping, then sub-word garbage) — `no_repeat_ngram_size: 3` with
        `repetition_penalty: 1.05` is the measured fix for a repeating thought.

        The record's own `system_prompt` (a non-empty string; `null` clears it) replaces
        the default block on EVERY turn of this agent — a main turn, a nudge, a turn a reply
        wakes — read live each turn; a per-call `system_prompt` beats it for that delivery.

        ## The KV-cache session table

        vLLM's prefix cache makes a warm conversation answer several times faster; past the
        server's KV capacity it preempts, and a preempted conversation re-reads its whole
        prompt. `kv_slots` keeps the conversations this kernel drives under that ceiling, and
        `kv_overflow` decides who gives way. ONE table per endpoint, shared by every
        vllm_backend agent in the kernel; `reflect` shows it (`kv.live`, `kv.conversations`
        least-recently used first, `kv.evicted`, `kv.rejected`).

        It is bookkeeping, not memory: vLLM owns the cache, and a rotated-out conversation
        that returns still hits whatever blocks vLLM kept. It sees only this kernel's
        traffic, so size `kv_slots` under what the server really holds.

        What keeps the cache hitting is a byte-stable PREFIX: an append-only history under an
        unchanged system prompt. A conversation is identified by its system text + first user
        turn; when compaction rewrites that prefix it is, correctly, a new conversation.

        ## Thinking

        The thought streams as `reasoning_content` and is wrapped once in `<think>…</think>`
        (parsed for tool calls, stripped before the turn is persisted). `thinking_token_budget`
        is vLLM's hard stop — once spent, the model moves on to its answer — so it must stay
        BELOW `max_tokens` (refused at the write otherwise). On a main turn an explicit budget
        is sent as set and the default is `min(16000, max_tokens // 2)`; a smaller side call (a
        fold summary) gets `min(budget, its max_tokens // 2)`; below 256 no budget is sent at all
        — a few tokens of thinking is no thinking, and a model starved of it stops with an
        empty answer. The value sent is `last_thinking_budget` in reflect.

        ## The key

        A door in front of vLLM may want one (`Authorization: Bearer <key>`). Name it with
        `credentials: {store, ref}` — a ref in a `credential_store`, read before each request
        and sent only to the ref's host. No `credentials` on the record, no header goes out,
        so a vLLM started without `--api-key` works as it is.

        ## Failures, in words

        - connection refused → vLLM is not listening yet (it loads for ~75 s after boot), or the tunnel is down
        - no answer within `connect_timeout` → the instance is stopped, or the tunnel/port is wrong
        - HTTP 401/403 → the door wants a key: name it in `credentials` (`{store, ref}`)
        - the stream drops, or ends without a `finish_reason` → vLLM died mid-answer; the turn is not complete
        - `finish_reason: length` → a cut (`last_stop_reason: max_tokens`), so the caller can continue

        """#
}
