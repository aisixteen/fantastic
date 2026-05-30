// web_ws — WebSocket verb-invocation surface as a child of `web`.
//
// Mirrors Python's `web_ws` bundle: it runs no server. It is a
// route-contributor — at host boot, the parent `web` pulls this
// agent's `get_routes` and mounts the returned descriptor. The actual
// WS handling is the host's shared proxy (`FantasticWeb.runWebSocketProxy`,
// the analog of Python's `web/_proxy.run`); this bundle only declares
// the `/{host_id}/ws` route.
//
// Opt-in: a `web` agent serves WS only when a `web_ws` child exists —
// exactly like Python. Create one with:
//   <web_id> create_agent handler_module=web_ws.tools

import FantasticJSON
import FantasticKernel
import Foundation

public let HANDLER_MODULE = "web_ws.tools"

public final class WebWSBundle: AgentBundle, @unchecked Sendable {
    public let name = "web_ws"
    public init() {}

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
                "kind": .string("web_ws"),
                "sentence": .string(
                    "WebSocket verb-invocation surface — contributes /<host_id>/ws to the parent web; handled by the host's shared proxy."
                ),
                "verbs": [
                    "get_routes":
                        "Returns {routes:[{kind:'websocket', method:'GET', path:'/{host_id}/ws'}]} — the parent web mounts it at boot.",
                    "boot": "No-op (no own server state).",
                    "shutdown": "No-op.",
                ] as JSON,
            ] as JSON
        case "get_routes":
            // `{host_id}` is a path template captured by the host's
            // route matcher — one web_ws serves WS for every agent id,
            // matching Python's `path: "/{host_id}/ws"`.
            return .object([
                "routes": .array([
                    .object([
                        "kind": .string("websocket"),
                        "method": .string("GET"),
                        "path": .string("/{host_id}/ws"),
                    ])
                ])
            ])
        case "boot", "shutdown", "stop":
            return .object(["ok": .bool(true)])
        default:
            return .object(["error": .string("unknown verb \(verb)")])
        }
    }
}
