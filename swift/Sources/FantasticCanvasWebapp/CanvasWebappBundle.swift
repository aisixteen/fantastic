// Spatial UI front-end (canvas).
//
// Mirrors Rust's `fantastic-canvas-webapp::CanvasWebappBundle`.
// Serves the canvas.html (loaded as a SwiftPM Resource) at `/<id>/`
// via the web bundle's `render_html` route.

import FantasticJSON
import FantasticKernel
import Foundation

public let HANDLER_MODULE = "canvas_webapp.tools"

public struct CanvasWebappBundle: AgentBundle {
    public let name = "canvas_webapp"
    public init() {}

    /// Bundled canvas.html resource, loaded once at first access.
    private static let html: String = {
        if let url = Bundle.module.url(
            forResource: "canvas", withExtension: "html"),
            let data = try? Data(contentsOf: url),
            let s = String(data: data, encoding: .utf8)
        {
            return s
        }
        return "<!doctype html><html><body>canvas.html unavailable</body></html>"
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
                "kind": .string("canvas_webapp"),
                "sentence": .string(
                    "Canvas webapp — serves canvas.html and pairs with a canvas_backend via upstream_id."),
                "upstream_id": kernel.agent(agentId)?.metaValue(forKey: "upstream_id")
                    ?? .null,
                "verbs": [
                    "render_html": "Returns the canvas surface HTML.",
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
                "kind": .string("canvas_webapp"),
                "url": .string("/\(agentId.value)/"),
            ] as JSON
        default:
            return .object(["error": .string("unknown verb \(verb)")])
        }
    }

    /// Idempotently ensure a `canvas_backend.tools` exists as a child of
    /// this webapp + bind its id into `upstream_id` meta. Ports the Rust
    /// `canvas_webapp::boot_reply` (rust/crates/bundles/fantastic-canvas-webapp/
    /// src/lib.rs:103-160) — without this, dropping a fresh canvas_webapp
    /// into a workdir gives the user a page that can't accept members
    /// because every consumer routes through the missing upstream.
    private func bootReply(agentId: AgentId, kernel: Kernel) async -> JSON {
        let backendHM = "canvas_backend.tools"

        guard let me = kernel.agent(agentId) else { return .null }

        // Already bound? upstream_id pointing at a live canvas_backend → no-op.
        if let upstreamStr = me.metaValue(forKey: "upstream_id")?.asString {
            let upstreamId = AgentId(upstreamStr)
            if kernel.agent(upstreamId) != nil {
                return .null
            }
        }

        // Or a backend already attached as a child (rehydrated from disk)?
        let hasBackendChild = me.childIds().contains { cid in
            kernel.agent(cid)?.handlerModule == backendHM
        }
        if hasBackendChild {
            return .null
        }

        // Spawn one as our child.
        let createReply = await kernel.send(
            agentId,
            .object([
                "type": .string("create_agent"),
                "handler_module": .string(backendHM),
            ]))
        guard let backendId = createReply["id"].asString else {
            return .object([
                "error": .string(
                    "canvas_webapp.boot: create backend failed: \(createReply.serialize())")
            ])
        }

        // Record the binding on this webapp's record so the page + canvas
        // chrome can discover the pair without walking the children dict.
        let updateReply = await kernel.send(
            AgentId("core"),
            .object([
                "type": .string("update_agent"),
                "id": .string(agentId.value),
                "upstream_id": .string(backendId),
            ]))
        if let err = updateReply["error"].asString {
            return .object([
                "error": .string("canvas_webapp.boot: write upstream_id failed: \(err)")
            ])
        }
        return .null
    }
}
