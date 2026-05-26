// Telemetry visualization pane.
//
// Mirrors Rust's `fantastic-telemetry-pane::TelemetryPaneBundle`.
// Subscribes to kernel state events + renders a live agent-vis
// surface. The Swift port keeps the verb shape; rendering happens
// client-side via the bundled visualization HTML.

import FantasticJSON
import FantasticKernel
import Foundation

public let HANDLER_MODULE = "telemetry_pane.tools"

public struct TelemetryPaneBundle: AgentBundle {
    public let name = "telemetry_pane"
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
                "kind": .string("telemetry_pane"),
                "sentence": .string("Live agent-vis surface — subscribe to kernel state events."),
                "verbs": [
                    "render_html": "Returns the vis surface HTML.",
                    "get_webapp": "Returns iframe descriptor.",
                ] as JSON,
            ] as JSON
        case "boot", "shutdown":
            return .object(["ok": .bool(true)])
        case "render_html":
            return .object([
                "html": .string(
                    "<!doctype html><html><body><div id=\"telemetry\"></div><script src=\"/_fantastic/transport.js\"></script></body></html>"
                )
            ])
        case "get_webapp":
            return [
                "kind": .string("telemetry_pane"),
                "url": .string("/\(agentId.value)/"),
            ] as JSON
        default:
            return .object(["error": .string("unknown verb \(verb)")])
        }
    }
}
