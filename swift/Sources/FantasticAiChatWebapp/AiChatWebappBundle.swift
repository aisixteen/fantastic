// Provider-agnostic chat UI.
//
// Mirrors Rust's `fantastic-ai-chat-webapp::AiChatWebappBundle`.
// Itself a webapp surface; routes chat verbs to an `upstream_id`
// (an LLM backend agent — ollama, NVIDIA NIM, FM proxy_agent, etc.).

import FantasticJSON
import FantasticKernel
import Foundation

public let HANDLER_MODULE = "ai_chat_webapp.tools"

public struct AiChatWebappBundle: AgentBundle {
    public let name = "ai_chat_webapp"
    public init() {}

    private static let defaultHtml = """
        <!doctype html><html><head><meta charset="utf-8"><title>chat</title></head>
        <body><div id="chat"></div><script src="/_fantastic/transport.js"></script>
        <script>
        const t = fantastic_transport();
        // Minimal stub — full chat UI loads dynamically in production.
        </script></body></html>
        """

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
                "kind": .string("ai_chat_webapp"),
                "sentence": .string(
                    "Chat UI — fronts any LLM backend via upstream_id meta field."),
                "upstream_id": agent.metaValue(forKey: "upstream_id") ?? .null,
                "provider": .string("agnostic"),
                "verbs": [
                    "render_html": "Returns chat UI HTML.",
                    "send": "args: text. Forwards to upstream backend.",
                    "history": "Forwards to upstream backend.",
                    "interrupt": "Forwards to upstream backend.",
                ] as JSON,
            ] as JSON
        case "boot":
            return await bootReply(agentId: agent.id, kernel: kernel)
        case "shutdown":
            return .object(["ok": .bool(true)])
        case "render_html":
            return .object(["html": .string(Self.defaultHtml)])
        case "get_webapp":
            return [
                "kind": .string("ai_chat_webapp"),
                "url": .string("/\(agent.id.value)/"),
            ] as JSON
        case "send", "history", "interrupt", "backend_state":
            // Forward to upstream backend if configured.
            guard let upstream = agent.metaValue(forKey: "upstream_id")?.asString else {
                return .object([
                    "error": .string("no upstream_id configured"),
                    "reason": .string("no_upstream"),
                ])
            }
            return await kernel.send(AgentId(upstream), payload)
        default:
            return .object(["error": .string("unknown verb \(verb)")])
        }
    }

    /// Idempotently ensure an LLM backend exists as a child of this
    /// webapp + bind its id into `upstream_id` meta. Backend handler
    /// module is provider-dependent: `meta.provider == "ollama"` →
    /// `ollama_backend.tools`; anything else → `nvidia_nim_backend.tools`.
    /// Ports `rust/crates/bundles/fantastic-ai-chat-webapp/src/lib.rs:132-186`.
    private func bootReply(agentId: AgentId, kernel: Kernel) async -> JSON {
        guard let me = kernel.agent(agentId) else { return .null }
        let provider = me.metaValue(forKey: "provider")?.asString ?? "ollama"
        let backendHM = (provider == "ollama")
            ? "ollama_backend.tools"
            : "nvidia_nim_backend.tools"

        if let upstreamStr = me.metaValue(forKey: "upstream_id")?.asString,
            !upstreamStr.isEmpty,
            kernel.agent(AgentId(upstreamStr)) != nil
        {
            return .object([
                "ok": .bool(true),
                "upstream_id": .string(upstreamStr),
            ])
        }
        let hasBackendChild = me.childIds().contains { cid in
            kernel.agent(cid)?.handlerModule == backendHM
        }
        if hasBackendChild {
            return .null
        }

        let createReply = await kernel.send(
            agentId,
            .object([
                "type": .string("create_agent"),
                "handler_module": .string(backendHM),
            ]))
        guard let backendId = createReply["id"].asString else {
            return .object([
                "error": .string(
                    "ai_chat_webapp.boot: create backend failed: \(createReply.serialize())")
            ])
        }
        let updateReply = await kernel.send(
            AgentId("core"),
            .object([
                "type": .string("update_agent"),
                "id": .string(agentId.value),
                "upstream_id": .string(backendId),
            ]))
        if let err = updateReply["error"].asString {
            return .object([
                "error": .string("ai_chat_webapp.boot: write upstream_id failed: \(err)")
            ])
        }
        return .null
    }
}
