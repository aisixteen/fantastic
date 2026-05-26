// GL-view-as-record: GL fragment / vertex source stored in agent meta.
//
// Mirrors Rust's `fantastic-gl-agent::GlAgentBundle`. The agent's
// `gl_source` (or `source`) meta field IS the shader/scene the GL
// host runs; canvas-webapp picks it up via `reflect`.

import FantasticJSON
import FantasticKernel
import Foundation

public let HANDLER_MODULE = "gl_agent.tools"

public struct GlAgentBundle: AgentBundle {
    public let name = "gl_agent"
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
            let source = agent.metaValue(forKey: "gl_source")?.asString
                ?? agent.metaValue(forKey: "source")?.asString ?? ""
            return [
                "id": .string(agent.id.value),
                "sentence": .string("GL agent — meta.gl_source IS the scene."),
                "kind": .string("gl_agent"),
                "gl_source": .string(source),
                "source": .string(source),
                "verbs": [
                    "set_source": "args: gl_source. Updates meta.gl_source.",
                ] as JSON,
            ] as JSON
        case "boot", "shutdown":
            return .object(["ok": .bool(true)])
        case "set_source":
            guard let src = (payload["gl_source"].asString ?? payload["source"].asString) else {
                return .object(["error": .string("set_source requires gl_source or source")])
            }
            agent.updateMeta(["gl_source": .string(src)])
            try? Persistence.persist(agent: agent, storage: kernel.storage)
            return .object(["ok": .bool(true)])
        default:
            return .object(["error": .string("unknown verb \(verb)")])
        }
    }
}
