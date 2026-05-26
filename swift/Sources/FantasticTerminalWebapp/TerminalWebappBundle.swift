// Terminal UI front-end (xterm).
//
// Mirrors Rust's `fantastic-terminal-webapp::TerminalWebappBundle`.
// Serves terminal index.html at `/<id>/`.

import FantasticJSON
import FantasticKernel
import Foundation

public let HANDLER_MODULE = "terminal_webapp.tools"

public struct TerminalWebappBundle: AgentBundle {
    public let name = "terminal_webapp"
    public init() {}

    private static let html: String = {
        if let url = Bundle.module.url(forResource: "index", withExtension: "html"),
            let data = try? Data(contentsOf: url),
            let s = String(data: data, encoding: .utf8)
        {
            return s
        }
        return "<!doctype html><html><body>terminal index.html unavailable</body></html>"
    }()

    public func handle(
        agentId: AgentId,
        payload: JSON,
        kernel: Kernel
    ) async throws -> JSON? {
        let verb = payload["type"].asString ?? ""
        switch verb {
        case "reflect":
            return [
                "id": .string(agentId.value),
                "kind": .string("terminal_webapp"),
                "upstream_id": kernel.agent(agentId)?.metaValue(forKey: "upstream_id") ?? .null,
                "verbs": [
                    "render_html": "Returns the terminal xterm surface.",
                    "get_webapp": "Returns iframe descriptor + URL.",
                ] as JSON,
            ] as JSON
        case "boot":
            return await bootReply(agentId: agentId, kernel: kernel)
        case "shutdown":
            return .object(["ok": .bool(true)])
        case "render_html":
            return .object(["html": .string(Self.html)])
        case "get_webapp":
            return [
                "kind": .string("terminal_webapp"),
                "url": .string("/\(agentId.value)/"),
            ] as JSON
        default:
            return .object(["error": .string("unknown verb \(verb)")])
        }
    }

    /// Idempotently ensure a `terminal_backend.tools` exists as a child
    /// of this webapp + bind its id into `upstream_id` meta. Ports
    /// `rust/crates/bundles/fantastic-terminal-webapp/src/lib.rs:116-170`.
    private func bootReply(agentId: AgentId, kernel: Kernel) async -> JSON {
        let backendHM = "terminal_backend.tools"

        guard let me = kernel.agent(agentId) else { return .null }

        if let upstreamStr = me.metaValue(forKey: "upstream_id")?.asString,
            kernel.agent(AgentId(upstreamStr)) != nil
        {
            return .null
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
                    "terminal_webapp.boot: create backend failed: \(createReply.serialize())")
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
                "error": .string("terminal_webapp.boot: write upstream_id failed: \(err)")
            ])
        }
        return .null
    }
}
