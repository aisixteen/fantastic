// HTTP host bundle.
//
// Serves /, /<id>/, /<id>/file/<path>, transport.js, and vendored
// /_assets/* over a fully live Network.framework HTTP listener
// (see HTTPServer.swift). No Hummingbird, NIO, or Rust required.
//
// Vendored third-party assets (Three.js, xterm, xterm-addon-fit)
// ship as SwiftPM resources alongside the transport.js client
// runtime. Verb surfaces (web_ws / web_rest) are sub-agents mounted
// via get_routes at boot.

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

    /// Live `WebServer` instances keyed by agent id. Created in `boot`,
    /// dropped by `stop` and by the `onShutdown`/`onDelete` hooks.
    /// NSLock-protected because boot/stop can race with concurrent verb
    /// dispatch.
    private let serversLock = NSLock()
    private var servers: [AgentId: WebServer] = [:]

    private func serverFor(_ id: AgentId) -> WebServer? {
        serversLock.lock(); defer { serversLock.unlock() }
        return servers[id]
    }

    /// Stop + forget the listener for `id`. `false` when none was running.
    /// Python `_stop_uvicorn`.
    private func stopListener(_ id: AgentId) -> Bool {
        guard let server = serverFor(id) else { return false }
        server.stop()
        setServer(nil, for: id)
        return true
    }

    /// The process is stopping: the listener dies with it. Python `on_delete`
    /// (its only hook, which the shutdown walk falls back to).
    public func onShutdown(agentId: AgentId, kernel: Kernel) async throws {
        _ = stopListener(agentId)
    }

    /// Cascade hook — the HTTP server does not outlive the agent record.
    public func onDelete(agentId: AgentId, kernel: Kernel) async throws {
        _ = stopListener(agentId)
    }

    private func setServer(_ server: WebServer?, for id: AgentId) {
        serversLock.lock(); defer { serversLock.unlock() }
        if let server = server {
            servers[id] = server
        } else {
            servers.removeValue(forKey: id)
        }
    }

    // ── Route-provider composition (mirrors Python's get_routes pull) ──

    /// Walk the web agent's children, pull each one's `get_routes`, and
    /// mount the returned specs. Returns the ids of children that
    /// contributed at least one route.
    private func mountAllSurfaces(server: WebServer, webId: AgentId, kernel: Kernel)
        async -> [AgentId]
    {
        guard let web = kernel.agent(webId) else { return [] }
        var mounted: [AgentId] = []
        for child in web.childIds() {
            if await mountSurface(server: server, childId: child, kernel: kernel) {
                mounted.append(child)
            }
        }
        return mounted
    }

    /// Pull one child's `get_routes` and mount the returned specs
    /// (unmount-first). Returns whether any route was mounted. Children
    /// that don't answer `get_routes` are silently skipped (weak).
    @discardableResult
    private func mountSurface(server: WebServer, childId: AgentId, kernel: Kernel)
        async -> Bool
    {
        let reply = await kernel.send([Item(childId, .object(["type": .string("get_routes")]))])[0].result
        guard let routes = reply["routes"].asArray, !routes.isEmpty else {
            return false
        }
        var specs: [RouteSpec] = []
        for r in routes {
            guard let kindStr = r["kind"].asString,
                let kind = RouteKind(rawValue: kindStr),
                let path = r["path"].asString
            else { continue }
            let method = r["method"].asString ?? "GET"
            specs.append(
                RouteSpec(kind: kind, method: method, path: path, ownerId: childId))
        }
        guard !specs.isEmpty else { return false }
        server.mountRoutes(specs, for: childId)
        return true
    }

    public var readme: String? {
        """
        web — HTTP rendering host.
        Serves /, /<id>/, /<id>/file/<path>, transport.js, vendored /_assets/*; verb surfaces (web_ws / web_rest) are sub-agents mounted via get_routes.
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
                "kind": .string("web"),
                "sentence": .string(
                    "HTTP host — rendering routes baked in; call surfaces mounted from sub-agents."
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
                // Python's VERBS table, first doc line each. `shutdown` is a
                // lifecycle hook, never an advertised verb: an agent that
                // advertises it is TOLD by `shutdown_agents`, and a listener
                // must not be stopped by the drain it is carrying.
                "verbs": [
                    "reflect": "Identity + uvicorn port + running flag + currently mounted surfaces (process-local; reflect the live serve for truth).",
                    "boot": "Idempotent. Builds the host on rec.port (required, no default), mounts each",
                    "stop": "No args. Asks uvicorn to shut down; cancels its task. Returns {stopped:bool}.",
                    "mount": "args: child_id:str (req). Pulls `get_routes` from the named child and (re)mounts its routes onto the live app. Idempotent — unmounts any prior routes for the child first.",
                    "unmount": "args: child_id:str (req). Removes every route belonging to the named child from the live app.",
                ] as JSON,
                "emits": [
                    "—": "rendering host emits nothing; surfaces emit on their own inboxes"
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
            do {
                let port = try server.start(portHint: portHint)
                setServer(server, for: agent.id)
                // Walk this web's children and mount each one's
                // contributed routes (analog of Python's
                // `_mount_all_surfaces`). The host is rendering-only;
                // WS/REST surfaces are composable children.
                let surfaces = await mountAllSurfaces(
                    server: server, webId: agent.id, kernel: kernel)
                return .object([
                    "ok": .bool(true),
                    "running": .bool(true),
                    "port": .integer(Int64(port)),
                    "surfaces": .array(surfaces.map { .string($0.value) }),
                ])
            } catch {
                return .object([
                    "error": .string("web boot failed: \(error)"),
                    "reason": .string("port_bind_failed"),
                ])
            }
        case "mount":
            // args: child_id. (Re)pull a single child's get_routes and
            // remount onto the live server. Hot-swap after boot.
            guard let childStr = payload["child_id"].asString else {
                return .object(["error": .string("web.mount: child_id required")])
            }
            guard let server = serverFor(agent.id) else {
                return .object(["error": .string("web.mount: not running")])
            }
            let mounted = await mountSurface(
                server: server, childId: AgentId(childStr), kernel: kernel)
            return .object([
                "mounted": .bool(mounted),
                "child_id": .string(childStr),
            ])
        case "unmount":
            guard let childStr = payload["child_id"].asString else {
                return .object(["error": .string("web.unmount: child_id required")])
            }
            if let server = serverFor(agent.id) {
                server.unmountRoutes(for: AgentId(childStr))
            }
            return .object(["unmounted": .bool(true), "child_id": .string(childStr)])
        case "shutdown":
            // A lifecycle no-op; the listener stops in `onShutdown`/`onDelete`.
            return .null
        case "stop":
            let stopped = stopListener(agent.id)
            agent.updateMeta(["running": .bool(false)])
            return .object(["stopped": .bool(stopped)])
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
            let renderReply = await kernel.send([Item(AgentId(id), .object(["type": .string("render_html")]))])[0].result
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
