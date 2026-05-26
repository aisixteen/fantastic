// HTTP host bundle.
//
// Mirrors Rust's `fantastic-web::WebBundle` at the verb / data
// shape level. Vendored third-party assets (Three.js, xterm,
// xterm-addon-fit) ship as SwiftPM resources alongside the
// transport.js client runtime.
//
// Phase 4 ships:
//   - Verb-level bundle (boot / shutdown / reflect)
//   - Vendored asset accessors (Three.js, xterm.*, transport.js)
//   - Route descriptor list for downstream consumers
//
// The live HTTP listener (Hummingbird- or NIO-backed) is a separate
// polish pass — during the dual-kernel migration (Phase 8), the
// existing Rust XCFramework continues to serve HTTP. Apps wanting
// to drive the Swift kernel directly can use the asset accessors +
// route table to wire their own server.

import FantasticJSON
import FantasticKernel
import Foundation

public let HANDLER_MODULE = "web.tools"

// ── Vendored asset accessors ──────────────────────────────────────

public enum WebAssets {
    public static var threeJS: String { load("three.module", ext: "js") }
    public static var xtermJS: String { load("xterm.min", ext: "js") }
    public static var xtermCSS: String { load("xterm.min", ext: "css") }
    public static var xtermAddonFitJS: String { load("xterm-addon-fit.min", ext: "js") }
    public static var transportJS: String { load("transport", ext: "js") }

    /// Standard route table for the kernel's HTTP surface.
    /// The actual listener implementation consumes this list when
    /// wiring an HTTP server; downstream Apple-platform apps can use
    /// the same mapping for an in-app embedded server.
    public static let routes: [(path: String, contentType: String)] = [
        ("/_assets/three.module.js", "application/javascript"),
        ("/_assets/xterm.min.js", "application/javascript"),
        ("/_assets/xterm.min.css", "text/css"),
        ("/_assets/xterm-addon-fit.min.js", "application/javascript"),
        ("/_fantastic/transport.js", "application/javascript"),
    ]

    /// Look up the asset body for a top-level static-asset URL path.
    /// Returns `nil` if the path isn't one of our vendored assets.
    public static func body(forPath path: String) -> (body: String, contentType: String)? {
        switch path {
        case "/_assets/three.module.js":
            return (threeJS, "application/javascript")
        case "/_assets/xterm.min.js":
            return (xtermJS, "application/javascript")
        case "/_assets/xterm.min.css":
            return (xtermCSS, "text/css")
        case "/_assets/xterm-addon-fit.min.js":
            return (xtermAddonFitJS, "application/javascript")
        case "/_fantastic/transport.js":
            return (transportJS, "application/javascript")
        default:
            return nil
        }
    }

    /// Inject `<script src="/_fantastic/transport.js"></script>` before `</head>`
    /// (or at the top of `<body>` if no head). Matches Rust's
    /// `inject_transport`.
    public static func injectTransport(into html: String) -> String {
        let injection = "<script src=\"/_fantastic/transport.js\"></script>"
        if html.contains(injection) { return html }
        if let range = html.range(of: "</head>") {
            return html.replacingCharacters(in: range, with: "\(injection)</head>")
        }
        if let range = html.range(of: "<body") {
            let withScript = html.replacingCharacters(in: range, with: "\(injection)<body")
            return withScript
        }
        return injection + html
    }

    private static func load(_ resource: String, ext: String) -> String {
        guard let url = Bundle.module.url(forResource: resource, withExtension: ext),
            let data = try? Data(contentsOf: url),
            let s = String(data: data, encoding: .utf8)
        else {
            return ""
        }
        return s
    }
}

// ── Bundle ─────────────────────────────────────────────────────────

public final class WebBundle: AgentBundle, @unchecked Sendable {
    public let name = "web"
    public init() {}

    /// Live `WebServer` instances keyed by agent id. Created in
    /// `boot`, dropped in `shutdown`. NSLock-protected because
    /// boot/shutdown can race with concurrent verb dispatch.
    private let serversLock = NSLock()
    private var servers: [AgentId: WebServer] = [:]

    private func serverFor(_ id: AgentId) -> WebServer? {
        serversLock.lock(); defer { serversLock.unlock() }
        return servers[id]
    }

    private func setServer(_ server: WebServer?, for id: AgentId) {
        serversLock.lock(); defer { serversLock.unlock() }
        if let server = server {
            servers[id] = server
        } else {
            servers.removeValue(forKey: id)
        }
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
                "kind": .string("web"),
                "sentence": .string(
                    "HTTP host — exposes /<id>/, /_fantastic/transport.js, /_assets/* + per-child get_routes."
                ),
                "port": agent.metaValue(forKey: "port") ?? .null,
                "running": agent.metaValue(forKey: "running") ?? .bool(false),
                "routes": .array(
                    WebAssets.routes.map {
                        .object([
                            "path": .string($0.path),
                            "content_type": .string($0.contentType),
                        ])
                    }),
                "verbs": [
                    "boot": "Marks the agent running (real HTTP listener wired by host).",
                    "shutdown": "Marks the agent stopped.",
                    "render": "args: agent_id. Returns the agent's render_html reply + transport.js injection.",
                    "asset": "args: path. Returns the vendored asset for /_assets/* or /_fantastic/transport.js.",
                ] as JSON,
            ] as JSON
        case "boot":
            // Skip if already running.
            if serverFor(agent.id) != nil {
                return .object([
                    "ok": .bool(true),
                    "running": .bool(true),
                    "port": .integer(Int64(kernel.httpPort())),
                ])
            }
            let portHint = UInt16(agent.metaValue(forKey: "port")?.asInt ?? 0)
            let server = WebServer(kernel: kernel, agentId: agent.id)
            installWebSocketUpgrade(on: server, kernel: kernel)
            do {
                let port = try server.start(portHint: portHint)
                setServer(server, for: agent.id)
                return .object([
                    "ok": .bool(true),
                    "running": .bool(true),
                    "port": .integer(Int64(port)),
                ])
            } catch {
                return .object([
                    "error": .string("web boot failed: \(error)"),
                    "reason": .string("port_bind_failed"),
                ])
            }
        case "shutdown", "stop":
            if let server = serverFor(agent.id) {
                server.stop()
                setServer(nil, for: agent.id)
            }
            agent.updateMeta(["running": .bool(false)])
            return .object(["ok": .bool(true)])
        case "asset":
            guard let path = payload["path"].asString else {
                return .object(["error": .string("asset requires path")])
            }
            guard let entry = WebAssets.body(forPath: path) else {
                return .object([
                    "error": .string("no asset for \(path)"),
                    "reason": .string("not_found"),
                ])
            }
            return .object([
                "content_type": .string(entry.contentType),
                "body": .string(entry.body),
            ])
        case "render":
            guard let id = payload["agent_id"].asString else {
                return .object(["error": .string("render requires agent_id")])
            }
            let renderReply = await kernel.send(
                AgentId(id), .object(["type": .string("render_html")]))
            guard let html = renderReply["html"].asString else {
                return renderReply
            }
            return .object([
                "html": .string(WebAssets.injectTransport(into: html))
            ])
        default:
            return .object(["error": .string("unknown verb \(verb)")])
        }
    }
}
