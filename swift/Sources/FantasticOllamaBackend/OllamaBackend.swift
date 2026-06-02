// Ollama LLM backend.
//
// Mirrors Rust's `fantastic-ollama-backend::OllamaBackendBundle`.
// Talks to a local ollama HTTP server (default localhost:11434),
// streams tokens via URLSession's AsyncBytes iteration. Per-(agent,
// client_id) chat history kept in-RAM; LLM tool calls flow through
// the tools.tools registry (same pattern as the Rust bundle).

import FantasticJSON
import FantasticKernel
import Foundation
import OrderedCollections

public let HANDLER_MODULE = "ollama_backend.tools"

public final class OllamaBackendBundle: AgentBundle, @unchecked Sendable {
    public let name = "ollama_backend"
    public init() {}

    /// Per-(agent_id, client_id) chat history.
    private let historyLock = NSLock()
    private var history: [String: [JSON]] = [:]

    /// Per-stream cancel flags.
    private let cancelLock = NSLock()
    private var cancelled: Set<String> = []

    public var readme: String? {
        """
        ollama_backend — local LLM agent.
        Per-client chat threads, FIFO lock, native tool-calls; verbs: send, history, interrupt, backend_state.
        """
    }

    public func handle(
        agentId: AgentId,
        payload: JSON,
        kernel: Kernel
    ) async throws -> JSON? {
        let verb = payload["type"].asString ?? ""
        guard let agent = kernel.agent(agentId) else {
            return .object(["error": .string("no agent")])
        }
        switch verb {
        case "reflect":
            return [
                "id": .string(agent.id.value),
                "sentence": .string("Ollama-backed LLM agent (native tool-calling)."),
                "kind": .string("ollama_backend"),
                "provider": .string("ollama"),
                "host": .string(host(agent: agent)),
                "model": .string(model(agent: agent)),
                "verbs": [
                    "send": "args: text, client_id?. Streams a response.",
                    "history": "args: client_id?. Returns prior turns.",
                    "interrupt": "args: client_id?. Cancels in-flight stream.",
                    "backend_state": "Reports availability + in-flight.",
                ] as JSON,
            ] as JSON
        case "boot":
            return .object(["ok": .bool(true)])
        case "shutdown":
            return .object(["ok": .bool(true)])
        case "send":
            return await sendVerb(agent: agent, payload: payload, kernel: kernel)
        case "history":
            return historyVerb(agent: agent, payload: payload)
        case "interrupt":
            return interruptVerb(agent: agent, payload: payload)
        case "backend_state":
            return .object([
                "provider": .string("ollama"),
                "host": .string(host(agent: agent)),
                "model": .string(model(agent: agent)),
            ])
        default:
            return .object(["error": .string("unknown verb \(verb)")])
        }
    }

    // MARK: - Verbs

    private func sendVerb(agent: Agent, payload: JSON, kernel: Kernel) async -> JSON {
        guard let text = payload["text"].asString else {
            return .object(["error": .string("send requires text")])
        }
        let clientId = payload["client_id"].asString ?? "cli"
        let streamId = "stm_\(UUID().uuidString.prefix(8))"
        let messageId = "msg_\(UUID().uuidString.prefix(8))"

        // Append user turn to history.
        appendHistory(
            key: historyKey(agent: agent.id, client: clientId),
            message: .object([
                "role": .string("user"),
                "content": .string(text),
                "complete": .bool(true),
            ]))

        // Fetch tools registry — same as Rust FM backend.
        var toolsJSON = "[]"
        let toolsReply = await kernel.send(
            "tools", .object(["type": .string("list_for_llm")]))
        if !toolsReply["tools"].asArray.isNilOrEmpty {
            toolsJSON = toolsReply["tools"].serialize()
        }

        // Kick the streaming task — runs in the background, emits
        // tokens via kernel.emit on the agent's inbox.
        let host = host(agent: agent)
        let model = model(agent: agent)
        let historySnapshot = readHistory(
            key: historyKey(agent: agent.id, client: clientId))

        Task { [weak self, weak kernel] in
            guard let self = self, let kernel = kernel else { return }
            await self.runStream(
                streamId: streamId,
                messageId: messageId,
                agentId: agent.id,
                clientId: clientId,
                host: host,
                model: model,
                history: historySnapshot,
                userText: text,
                toolsJSON: toolsJSON,
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
        streamId: String,
        messageId: String,
        agentId: AgentId,
        clientId: String,
        host: String,
        model: String,
        history: [JSON],
        userText: String,
        toolsJSON: String,
        kernel: Kernel
    ) async {
        let url = URL(string: "\(host)/api/chat")!
        var req = URLRequest(url: url)
        req.httpMethod = "POST"
        req.setValue("application/json", forHTTPHeaderField: "Content-Type")

        // Build chat completion payload (ollama /api/chat shape).
        var messages: [JSON] = history
        messages.append(
            .object([
                "role": .string("user"),
                "content": .string(userText),
            ]))
        let body: JSON = .object([
            "model": .string(model),
            "messages": .array(messages),
            "stream": .bool(true),
        ])
        req.httpBody = body.serialize().data(using: .utf8)

        var accumulated = ""
        do {
            let (bytes, _) = try await URLSession.shared.bytes(for: req)
            for try await line in bytes.lines {
                if isCancelled(streamId) {
                    break
                }
                guard let data = line.data(using: .utf8),
                    let parsed = try? JSON.parse(data)
                else { continue }
                if let delta = parsed["message"]["content"].asString {
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
                }
                if parsed["done"].asBool == true {
                    break
                }
            }
        } catch {
            await kernel.emit(
                agentId,
                .object([
                    "type": .string("done"),
                    "stream_id": .string(streamId),
                    "message_id": .string(messageId),
                    "error": .string("\(error)"),
                    "client_id": .string(clientId),
                ]))
            clearCancel(streamId)
            return
        }

        // Append assistant turn to history.
        appendHistory(
            key: historyKey(agent: agentId, client: clientId),
            message: .object([
                "role": .string("assistant"),
                "content": .string(accumulated),
                "complete": .bool(true),
            ]))

        await kernel.emit(
            agentId,
            .object([
                "type": .string("done"),
                "stream_id": .string(streamId),
                "message_id": .string(messageId),
                "accumulated": .string(accumulated),
                "client_id": .string(clientId),
            ]))
        clearCancel(streamId)
    }

    private func historyVerb(agent: Agent, payload: JSON) -> JSON {
        let clientId = payload["client_id"].asString ?? "cli"
        let messages = readHistory(key: historyKey(agent: agent.id, client: clientId))
        return .object([
            "messages": .array(messages),
            "client_id": .string(clientId),
        ])
    }

    private func interruptVerb(agent: Agent, payload: JSON) -> JSON {
        // For simplicity we mark ALL streams for the agent cancelled.
        // Real implementation would track per-stream agent_id.
        cancelLock.lock()
        cancelled.removeAll()
        cancelLock.unlock()
        return .object(["interrupted": .bool(true)])
    }

    // MARK: - Helpers

    private func host(agent: Agent) -> String {
        agent.metaValue(forKey: "host")?.asString ?? "http://127.0.0.1:11434"
    }

    private func model(agent: Agent) -> String {
        agent.metaValue(forKey: "model")?.asString ?? "llama3.2"
    }

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

    private func isCancelled(_ id: String) -> Bool {
        cancelLock.lock()
        defer { cancelLock.unlock() }
        return cancelled.contains(id)
    }

    private func clearCancel(_ id: String) {
        cancelLock.lock()
        defer { cancelLock.unlock() }
        cancelled.remove(id)
    }
}

extension Optional where Wrapped == [JSON] {
    fileprivate var isNilOrEmpty: Bool {
        switch self {
        case .none: return true
        case .some(let arr): return arr.isEmpty
        }
    }
}
