// UI-as-record: HTML stored in agent meta.
//
// Mirrors Rust's `fantastic-html-agent::HtmlAgentBundle`. The agent's
// `html` meta field IS the rendered surface; the web bundle's
// `/<id>/` route picks up the `render_html` reply.

import FantasticJSON
import FantasticKernel
import Foundation

public let HANDLER_MODULE = "html_agent.tools"

public struct HtmlAgentBundle: AgentBundle {
    public let name = "html_agent"
    public init() {}

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
                "sentence": .string("HTML agent — meta.html IS the rendered surface."),
                "kind": .string("html_agent"),
                "verbs": [
                    "render_html": "Returns {html} from meta.html.",
                    "set_html": "args: html. Updates meta.html and re-renders.",
                ] as JSON,
            ] as JSON
        case "boot":
            return .object(["ok": .bool(true)])
        case "render_html":
            let html = agent.metaValue(forKey: "html")?.asString ?? "<!doctype html><html><body></body></html>"
            return .object(["html": .string(html)])
        case "set_html":
            guard let html = payload["html"].asString else {
                return .object(["error": .string("set_html requires html")])
            }
            agent.updateMeta(["html": .string(html)])
            try? Persistence.persist(agent: agent, storage: kernel.storage)
            return .object(["ok": .bool(true)])
        case "get_webapp":
            return [
                "kind": .string("html_agent"),
                "url": .string("/\(agent.id.value)/"),
            ] as JSON
        default:
            return .object(["error": .string("unknown verb \(verb)")])
        }
    }
}
