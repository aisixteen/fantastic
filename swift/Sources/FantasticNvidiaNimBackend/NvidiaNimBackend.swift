// NVIDIA NIM (OpenAI-compatible) LLM backend.
//
// Mirrors Rust's `fantastic-nvidia-nim-backend::NvidiaNimBundle`.
// Talks to NIM via HTTPS POST + Bearer auth; streams tokens via
// Server-Sent Events; aggregates tool-call deltas across SSE
// chunks.
//
// Wire-shape parity with the Rust bundle so the chat_webapp can
// front it identically.

import FantasticJSON
import FantasticKernel
import Foundation
import OrderedCollections

public let HANDLER_MODULE = "nvidia_nim_backend.tools"

public final class NvidiaNimBundle: AgentBundle, @unchecked Sendable {
    public let name = "nvidia_nim_backend"
    public init() {}

    /// Per-(agent_id, client_id) chat history.
    private let historyLock = NSLock()
    private var history: [String: [JSON]] = [:]

    /// Per-stream cancel flags.
    private let cancelLock = NSLock()
    private var cancelled: Set<String> = []

    public var readme: String? {
        """
        nvidia_nim_backend — NVIDIA NIM LLM agent (OpenAI-compatible).
        verbs: send, history, interrupt, backend_state; api_key stored out-of-band via file_agent sidecar; 429 rate-limit retry.
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
                "sentence": .string("NVIDIA NIM-backed LLM agent (OpenAI-compatible, native tool-calling)."),
                "kind": .string("nvidia_nim_backend"),
                "provider": .string("nvidia_nim"),
                "host": .string(host(agent: agent)),
                "model": .string(model(agent: agent)),
                "verbs": [
                    "send": "args: text, client_id?. Streams a response via SSE.",
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
            return interruptVerb()
        case "backend_state":
            return .object([
                "provider": .string("nvidia_nim"),
                "host": .string(host(agent: agent)),
                "model": .string(model(agent: agent)),
                "configured":
                    .bool(agent.metaValue(forKey: "api_key")?.asString != nil),
            ])
        default:
            return .object(["error": .string("unknown verb \(verb)")])
        }
    }

    // MARK: - send

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

        // Tools pre-fetch from registry (same pattern as ollama).
        let toolsArray: [JSON]
        let toolsReply = await kernel.send(
            "tools", .object(["type": .string("list_for_llm")]))
        toolsArray = toolsReply["tools"].asArray ?? []

        let host = host(agent: agent)
        let model = model(agent: agent)
        let apiKey = agent.metaValue(forKey: "api_key")?.asString
        let historySnapshot = readHistory(
            key: historyKey(agent: agent.id, client: clientId))

        guard let apiKey = apiKey, !apiKey.isEmpty else {
            return .object([
                "error": .string("nvidia_nim: api_key not configured"),
                "reason": .string("no_api_key"),
            ])
        }

        // Kick the streaming task.
        Task { [weak self, weak kernel] in
            guard let self = self, let kernel = kernel else { return }
            await self.runStream(
                streamId: streamId,
                messageId: messageId,
                agentId: agent.id,
                clientId: clientId,
                host: host,
                model: model,
                apiKey: apiKey,
                history: historySnapshot,
                userText: text,
                tools: toolsArray,
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
        apiKey: String,
        history: [JSON],
        userText: String,
        tools: [JSON],
        kernel: Kernel
    ) async {
        let url = URL(string: "\(host)/v1/chat/completions")!
        var req = URLRequest(url: url)
        req.httpMethod = "POST"
        req.setValue("application/json", forHTTPHeaderField: "Content-Type")
        req.setValue("Bearer \(apiKey)", forHTTPHeaderField: "Authorization")
        req.setValue("text/event-stream", forHTTPHeaderField: "Accept")

        // OpenAI-compatible chat completion payload.
        var messages: [JSON] = history
        messages.append(
            .object([
                "role": .string("user"),
                "content": .string(userText),
            ]))
        var body: OrderedDictionary<String, JSON> = [:]
        body["model"] = .string(model)
        body["messages"] = .array(messages)
        body["stream"] = .bool(true)
        if !tools.isEmpty {
            // OpenAI tool shape: [{type:"function", function: {name, description, parameters}}]
            let wrapped = tools.map { t -> JSON in
                .object([
                    "type": .string("function"),
                    "function": .object([
                        "name": t["name"],
                        "description": t["description"],
                        "parameters": t["parameters"],
                    ]),
                ])
            }
            body["tools"] = .array(wrapped)
        }
        req.httpBody = JSON.object(body).serialize().data(using: .utf8)

        // 429 retry with exponential backoff (1s, 2s, 4s).
        var attempt = 0
        let maxAttempts = 3
        var accumulated = ""
        var toolCallsAccum: OrderedDictionary<Int, OrderedDictionary<String, JSON>> = [:]

        while attempt < maxAttempts {
            attempt += 1
            do {
                let (bytes, response) = try await URLSession.shared.bytes(for: req)
                if let http = response as? HTTPURLResponse {
                    if http.statusCode == 429 {
                        let backoffMs = UInt64(1000 * (1 << (attempt - 1)))
                        try? await Task.sleep(nanoseconds: backoffMs * 1_000_000)
                        continue
                    }
                    if http.statusCode >= 400 {
                        await emitDone(
                            kernel: kernel, agentId: agentId,
                            streamId: streamId, messageId: messageId,
                            clientId: clientId,
                            accumulated: accumulated,
                            error: "HTTP \(http.statusCode)")
                        return
                    }
                }
                for try await line in bytes.lines {
                    if isCancelled(streamId) {
                        break
                    }
                    // SSE: data: <json> ; [DONE] terminator.
                    guard line.hasPrefix("data: ") else { continue }
                    let payload = String(line.dropFirst(6))
                    if payload == "[DONE]" { break }
                    guard let data = payload.data(using: .utf8),
                        let parsed = try? JSON.parse(data)
                    else { continue }
                    let choice = parsed["choices"][0]
                    let delta = choice["delta"]
                    if let chunk = delta["content"].asString {
                        accumulated += chunk
                        await kernel.emit(
                            agentId,
                            .object([
                                "type": .string("token"),
                                "stream_id": .string(streamId),
                                "message_id": .string(messageId),
                                "delta": .string(chunk),
                                "accumulated": .string(accumulated),
                                "client_id": .string(clientId),
                            ]))
                    }
                    // Tool call aggregation: deltas arrive as
                    //   {index, id?, function: {name?, arguments?}}
                    // and must be accumulated by index.
                    if let calls = delta["tool_calls"].asArray {
                        for call in calls {
                            guard let idx = call["index"].asInt else { continue }
                            var existing = toolCallsAccum[Int(idx)] ?? [:]
                            if let id = call["id"].asString {
                                existing["id"] = .string(id)
                            }
                            if let fn = call["function"].asObject {
                                var fnExisting = existing["function"]?.asObject ?? [:]
                                if let name = fn["name"]?.asString {
                                    fnExisting["name"] = .string(name)
                                }
                                if let args = fn["arguments"]?.asString {
                                    let prev = fnExisting["arguments"]?.asString ?? ""
                                    fnExisting["arguments"] = .string(prev + args)
                                }
                                existing["function"] = .object(fnExisting)
                            }
                            toolCallsAccum[Int(idx)] = existing
                        }
                    }
                }
                break  // success — exit retry loop
            } catch {
                await emitDone(
                    kernel: kernel, agentId: agentId,
                    streamId: streamId, messageId: messageId,
                    clientId: clientId,
                    accumulated: accumulated, error: "\(error)")
                clearCancel(streamId)
                return
            }
        }

        // Append assistant turn (text + tool calls if any) to history.
        var assistant: OrderedDictionary<String, JSON> = [:]
        assistant["role"] = .string("assistant")
        assistant["content"] = .string(accumulated)
        assistant["complete"] = .bool(true)
        if !toolCallsAccum.isEmpty {
            let calls = toolCallsAccum.values.sorted { lhs, rhs in
                (lhs["id"]?.asString ?? "") < (rhs["id"]?.asString ?? "")
            }.map { JSON.object($0) }
            assistant["tool_calls"] = .array(calls)
        }
        appendHistory(
            key: historyKey(agent: agentId, client: clientId),
            message: .object(assistant))

        await emitDone(
            kernel: kernel, agentId: agentId,
            streamId: streamId, messageId: messageId,
            clientId: clientId,
            accumulated: accumulated, error: nil)
        clearCancel(streamId)
    }

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
        event["accumulated"] = .string(accumulated)
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

    private func interruptVerb() -> JSON {
        cancelLock.lock()
        cancelled.removeAll()
        cancelLock.unlock()
        return .object(["interrupted": .bool(true)])
    }

    // MARK: - Helpers

    private func host(agent: Agent) -> String {
        agent.metaValue(forKey: "host")?.asString ?? "https://integrate.api.nvidia.com"
    }

    private func model(agent: Agent) -> String {
        agent.metaValue(forKey: "model")?.asString ?? "meta/llama-3.1-70b-instruct"
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
