// AIBackend — the shared reflect-driven LLM-agent machinery behind the
// `AIProvider` seam. The ollama / NIM / Apple-FM bundles supply only
// their `AIProvider` impl + an `AIBackendConfig`, and dispatch every
// verb through the `handle` method here.
//
// Extracted from the three (formerly duplicated) Swift backends. The
// shared layer owns: per-(agent, client_id) chat history, the
// epoch-bump cancellation state, the verb bodies (reflect / boot /
// shutdown / send / history / interrupt / backend_state), the spawned
// streaming task, and the token / done event emission. The providers
// own only the upstream wire (NDJSON, SSE+429+Bearer, on-device
// session).
//
// Wire contract (byte-identical to the Python reference, enforced by
// FantasticParityTests):
//
//   send → {queued, stream_id, message_id, client_id}
//   token event → {type:token, stream_id, message_id, delta,
//                  accumulated, client_id}
//   done  event → {type:done, stream_id, message_id, accumulated?,
//                  client_id, error?}
//   history → {messages, client_id}
//   interrupt → {interrupted:true}
//
// This module MUST NOT import FoundationModels (or any provider SDK).

import FantasticJSON
import FantasticKernel
import Foundation
import OrderedCollections

/// Construct the shared backend machinery for one bundle. Returns an
/// `AIBackend` the bundle's `handle` delegates straight to. Mirrors
/// Rust's `buildAIBackend` / the Python `ai_core` factory.
public func buildAIBackend(_ config: AIBackendConfig) -> AIBackend {
    AIBackend(config: config)
}

public final class AIBackend: @unchecked Sendable {
    public let config: AIBackendConfig

    /// Per-(agent_id, client_id) chat history.
    private let historyLock = NSLock()
    private var history: [String: [JSON]] = [:]

    /// Cancellation state — epoch-bump pattern (lifted verbatim from
    /// the FM backend, which had the most robust version). `interrupt`
    /// bumps `interruptEpoch`; every stream records the epoch it
    /// started under and polls `isStreamCancelled` each tick. A stream
    /// is cancelled iff its `startedAt` epoch is older than the current
    /// epoch OR its id is explicitly in `cancelledStreams`. Epoch
    /// counters are race-free against staggered stream-death and don't
    /// bleed into post-interrupt sends.
    private let cancelLock = NSLock()
    private var interruptEpoch: UInt64 = 0
    private var cancelledStreams: Set<String> = []

    /// In-flight stream count for `backend_state` reporting.
    private let stateLock = NSLock()
    private var inFlight: Int = 0

    init(config: AIBackendConfig) {
        self.config = config
    }

    // MARK: - Verb dispatch

    /// Single entry point — the bundle's `handle` forwards here.
    public func handle(
        agentId: AgentId,
        payload: JSON,
        kernel: Kernel
    ) async -> JSON? {
        let verb = payload["type"].asString ?? ""
        guard let agent = kernel.agent(agentId) else {
            return .object(["error": .string("no agent")])
        }
        switch verb {
        case "reflect":
            return reflectReply(agent: agent)
        case "boot":
            return .object(["ok": .bool(true)])
        case "shutdown":
            return .object(["ok": .bool(true)])
        case "send":
            return await sendVerb(agent: agent, payload: payload, kernel: kernel)
        case "history":
            return historyVerb(agent: agent, payload: payload)
        case "interrupt":
            return interruptVerb(payload: payload)
        case "backend_state":
            return backendStateReply(agent: agent)
        default:
            return .object(["error": .string("unknown verb \(verb)")])
        }
    }

    // MARK: - reflect / backend_state

    private func reflectReply(agent: Agent) -> JSON {
        var fields: OrderedDictionary<String, JSON> = [:]
        fields["id"] = .string(agent.id.value)
        fields["sentence"] = .string(config.sentence)
        fields["kind"] = .string(config.kind)
        fields["provider"] = .string(config.provider)
        for (k, v) in config.reflectExtra(agent) {
            fields[k] = v
        }
        fields["verbs"] = config.verbs
        return .object(fields)
    }

    private func backendStateReply(agent: Agent) -> JSON {
        var fields: OrderedDictionary<String, JSON> = [:]
        fields["provider"] = .string(config.provider)
        for (k, v) in config.backendStateExtra(agent) {
            fields[k] = v
        }
        return .object(fields)
    }

    // MARK: - send

    private func sendVerb(agent: Agent, payload: JSON, kernel: Kernel) async -> JSON {
        guard let text = payload["text"].asString else {
            return .object(["error": .string("send requires text")])
        }
        let clientId = payload["client_id"].asString ?? "cli"

        // Build the provider FIRST — backends that must refuse before
        // touching history (NIM no-api_key, FM unavailable) return their
        // exact refusal body here, preserving wire byte-identity.
        let result = await config.makeProvider(agent, clientId, kernel)
        let provider: any AIProvider
        switch result {
        case .refused(let body):
            return body
        case .provider(let p):
            provider = p
        }

        let streamId = "stm_\(UUID().uuidString.prefix(8))"
        let messageId = "msg_\(UUID().uuidString.prefix(8))"
        let userMessageId = "msg_\(UUID().uuidString.prefix(8))"

        // Append user turn to history. Stateless backends (FM) carry an
        // `id` on each row (UI bubble identity); ollama/NIM don't.
        appendHistory(
            key: historyKey(agent: agent.id, client: clientId),
            message: userTurn(id: userMessageId, text: text))

        // Pre-fetch tools registry — identical across all backends.
        let toolsReply = await kernel.send(
            "tools", .object(["type": .string("list_for_llm")]))
        let tools = toolsReply["tools"].asArray ?? []

        // Stateless backends do NOT feed prior history back as context.
        let historySnapshot =
            config.stateless
            ? []
            : readHistory(key: historyKey(agent: agent.id, client: clientId))

        bumpInFlight(+1)
        Task { [weak self, weak kernel] in
            guard let self = self, let kernel = kernel else { return }
            await self.runStream(
                provider: provider,
                streamId: streamId,
                messageId: messageId,
                agentId: agent.id,
                clientId: clientId,
                history: historySnapshot,
                userText: text,
                tools: tools,
                kernel: kernel
            )
        }

        return .object([
            "queued": .bool(true),
            "stream_id": .string(streamId),
            "message_id": .string(messageId),
            "client_id": .string(clientId),
        ])
    }

    private func runStream(
        provider: any AIProvider,
        streamId: String,
        messageId: String,
        agentId: AgentId,
        clientId: String,
        history: [JSON],
        userText: String,
        tools: [JSON],
        kernel: Kernel
    ) async {
        defer {
            clearStreamCancel(streamId)
            bumpInFlight(-1)
        }

        // Snapshot the cancel epoch before the first token so interrupts
        // that fire mid-stream are detected via `startedAt < epoch`.
        let startedAtEpoch = currentEpoch()

        // Assemble messages: prior history (empty when stateless) + the
        // new user turn (model shape — no `complete`/`id` bookkeeping).
        var messages: [JSON] = history
        messages.append(
            .object([
                "role": .string("user"),
                "content": .string(userText),
            ]))

        var accumulated = ""
        var toolCalls: [JSON] = []

        do {
            let stream = provider.chat(messages: messages, tools: tools)
            for try await chunk in stream {
                if isStreamCancelled(streamId: streamId, startedAt: startedAtEpoch) {
                    await provider.stop()
                    if config.emitInterruptedError {
                        // FM shape: terminal `done{error:"interrupted"}`,
                        // partial turn NOT persisted.
                        await emitDone(
                            kernel: kernel, agentId: agentId,
                            streamId: streamId, messageId: messageId,
                            clientId: clientId, accumulated: accumulated,
                            error: "interrupted")
                        return
                    }
                    break
                }
                switch chunk {
                case .token(let delta):
                    accumulated += delta
                    await kernel.emit(
                        agentId,
                        .object([
                            "type": .string("token"),
                            "stream_id": .string(streamId),
                            "message_id": .string(messageId),
                            "delta": .string(delta),
                            "accumulated": .string(accumulated),
                            "client_id": .string(clientId),
                        ]))
                case .toolCall(let call):
                    if config.persistToolCalls {
                        toolCalls.append(call)
                    }
                }
            }
        } catch {
            await emitDone(
                kernel: kernel, agentId: agentId,
                streamId: streamId, messageId: messageId,
                clientId: clientId, accumulated: accumulated,
                error: "\(error)")
            return
        }

        // Append assistant turn to history.
        appendHistory(
            key: historyKey(agent: agentId, client: clientId),
            message: assistantTurn(
                id: messageId, content: accumulated, toolCalls: toolCalls))

        await emitDone(
            kernel: kernel, agentId: agentId,
            streamId: streamId, messageId: messageId,
            clientId: clientId, accumulated: accumulated, error: nil)
    }

    /// Emit the terminal `done` event. On the error path, `accumulated`
    /// is included only when the backend historically did so (NIM).
    private func emitDone(
        kernel: Kernel,
        agentId: AgentId,
        streamId: String,
        messageId: String,
        clientId: String,
        accumulated: String,
        error: String?
    ) async {
        var event: OrderedDictionary<String, JSON> = [:]
        event["type"] = .string("done")
        event["stream_id"] = .string(streamId)
        event["message_id"] = .string(messageId)
        if error == nil || config.includeAccumulatedOnError {
            event["accumulated"] = .string(accumulated)
        }
        event["client_id"] = .string(clientId)
        if let error = error {
            event["error"] = .string(error)
        }
        await kernel.emit(agentId, .object(event))
    }

    // MARK: - history / interrupt

    private func historyVerb(agent: Agent, payload: JSON) -> JSON {
        let clientId = payload["client_id"].asString ?? "cli"
        let messages = readHistory(key: historyKey(agent: agent.id, client: clientId))
        return .object([
            "messages": .array(messages),
            "client_id": .string(clientId),
        ])
    }

    private func interruptVerb(payload: JSON) -> JSON {
        cancelLock.lock()
        interruptEpoch &+= 1
        cancelLock.unlock()
        _ = payload  // client_id reserved for per-stream cancel later
        return .object(["interrupted": .bool(true)])
    }

    // MARK: - history row shapes

    private func userTurn(id: String, text: String) -> JSON {
        var row: OrderedDictionary<String, JSON> = [:]
        if config.stateless { row["id"] = .string(id) }
        row["role"] = .string("user")
        row["content"] = .string(text)
        row["complete"] = .bool(true)
        return .object(row)
    }

    private func assistantTurn(id: String, content: String, toolCalls: [JSON]) -> JSON {
        var row: OrderedDictionary<String, JSON> = [:]
        if config.stateless { row["id"] = .string(id) }
        row["role"] = .string("assistant")
        row["content"] = .string(content)
        row["complete"] = .bool(true)
        if config.persistToolCalls && !toolCalls.isEmpty {
            // Sort by id to match the prior NIM ordering exactly.
            let sorted = toolCalls.sorted { lhs, rhs in
                (lhs["id"].asString ?? "") < (rhs["id"].asString ?? "")
            }
            row["tool_calls"] = .array(sorted)
        }
        return .object(row)
    }

    // MARK: - history helpers (NSLock-protected)

    private func historyKey(agent: AgentId, client: String) -> String {
        "\(agent.value)|\(client)"
    }

    private func appendHistory(key: String, message: JSON) {
        historyLock.lock()
        defer { historyLock.unlock() }
        var arr = history[key] ?? []
        arr.append(message)
        history[key] = arr
    }

    private func readHistory(key: String) -> [JSON] {
        historyLock.lock()
        defer { historyLock.unlock() }
        return history[key] ?? []
    }

    // MARK: - cancellation (epoch-bump) + in-flight

    /// Snapshot the current epoch — call at the top of a stream so the
    /// stream's birth epoch is fixed before the first cancel poll.
    public func currentEpoch() -> UInt64 {
        cancelLock.lock()
        defer { cancelLock.unlock() }
        return interruptEpoch
    }

    /// True iff the stream should bail: its id was explicitly cancelled,
    /// or an interrupt fired after it started (epoch advanced).
    public func isStreamCancelled(streamId: String, startedAt: UInt64) -> Bool {
        cancelLock.lock()
        defer { cancelLock.unlock() }
        return cancelledStreams.contains(streamId) || startedAt < interruptEpoch
    }

    private func clearStreamCancel(_ id: String) {
        cancelLock.lock()
        defer { cancelLock.unlock() }
        cancelledStreams.remove(id)
    }

    private func bumpInFlight(_ delta: Int) {
        stateLock.lock()
        defer { stateLock.unlock() }
        inFlight = max(0, inFlight + delta)
    }

    /// Current in-flight stream count (for `backend_state` reporting).
    public func readInFlight() -> Int {
        stateLock.lock()
        defer { stateLock.unlock() }
        return inFlight
    }
}
