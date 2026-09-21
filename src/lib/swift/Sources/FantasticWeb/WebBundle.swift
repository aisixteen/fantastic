// HTTP host bundle (python `web.tools`).
//
// `web` owns the listener. Rendering routes are baked in (`/`,
// `/_assets/favicon.png`, `/favicon.png`, `/<id>/file/<path>` — no
// server-side `/<id>/` render route, no transport.js, no vendored asset
// tree; all UI is the TS frontend). Verb-invocation routes are NOT —
// those live in sub-agent bundles (`web_ws`, `web_rest`) that declare
// their routes via the duck-typed `get_routes` verb. On boot, web walks
// its children and mounts whatever they return onto the live router.
//
// Verbs:
//   reflect   -> {sentence, port, served_agent_count, running, surfaces}
//   boot      -> start the listener; mount each child's surface
//   stop      -> stop the listener
//   mount     -> (re)mount a child's routes (idempotent)
//   unmount   -> remove a child's routes from the live router

import FantasticIoBridge
import FantasticJSON
import FantasticKernel
import Foundation
import OrderedCollections

public let HANDLER_MODULE = "web.tools"

public final class WebBundle: AgentBundle, @unchecked Sendable {
    public let name = "web"
    public var verbs: [String] { Array(Self.verbDocs.keys) }
    public init() {}

    /// Live `WebServer` instances keyed by agent id — python's `_servers`.
    /// Created in `boot`, dropped by `stop` and by the `onShutdown`/`onDelete`
    /// hooks. NSLock-protected because boot/stop can race with concurrent verb
    /// dispatch.
    private let serversLock = NSLock()
    private var servers: [AgentId: WebServer] = [:]

    private func serverFor(_ id: AgentId) -> WebServer? {
        serversLock.lock()
        defer { serversLock.unlock() }
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
        serversLock.lock()
        defer { serversLock.unlock() }
        if let server = server {
            servers[id] = server
        } else {
            servers.removeValue(forKey: id)
        }
    }

    // ── Route-provider composition (python's get_routes pull) ──

    /// Walk the web agent's children and mount each one's surface — python
    /// `_mount_all_surfaces`.
    private func mountAllSurfaces(server: WebServer, webId: AgentId, kernel: Kernel) async {
        guard let web = kernel.agent(webId) else { return }
        for child in web.childIds() {
            await mountSurface(server: server, childId: child, kernel: kernel)
        }
    }

    /// Pull one child's `get_routes` and mount the returned specs — python
    /// `_mount_surface`. A child that doesn't answer is silently skipped (weak).
    private func mountSurface(server: WebServer, childId: AgentId, kernel: Kernel) async {
        let reply = await kernel.send([Item(childId, .object(["type": .string("get_routes")]))])[0]
            .result
        guard case .object = reply else { return }
        let routes = reply["routes"].asArray ?? []
        var specs: [RouteSpec] = []
        for r in routes {
            guard let path = r["path"].asString, !path.isEmpty else { continue }
            // python keys the handler off `kind`; `websocket` runs the shared WS
            // proxy, anything else is an HTTP route whose method defaults to GET.
            let kind: RouteKind = r["kind"].asString == "websocket" ? .websocket : .http
            let method = (r["method"].asString ?? "GET").uppercased()
            specs.append(RouteSpec(kind: kind, method: method, path: path, ownerId: childId))
        }
        guard !specs.isEmpty else { return }
        server.mountRoutes(specs)
    }

    public var readme: String? { Self.readmeText }

    public func handle(
        agentId: AgentId,
        payload: JSON,
        kernel: Kernel
    ) async throws -> JSON? {
        let type = payload["type"]
        guard let agent = kernel.agent(agentId) else {
            return .object(["error": .string("no agent")])
        }
        switch type.asString {
        case "reflect":
            return reflect(agent: agent, kernel: kernel)
        case "boot":
            return try await boot(agent: agent, kernel: kernel)
        case "stop":
            return .object(["stopped": .bool(stopListener(agent.id))])
        case "mount":
            guard let childId = payload["child_id"].asString, !childId.isEmpty else {
                return .object(["error": .string("web.mount: child_id required")])
            }
            guard let server = serverFor(agent.id) else {
                return .object(["error": .string("web.mount: not running")])
            }
            // Idempotent: unmount-first, then re-pull.
            server.unmountRoutes(for: AgentId(childId))
            await mountSurface(server: server, childId: AgentId(childId), kernel: kernel)
            return .object([
                "mounted": .array(server.paths(for: AgentId(childId)).map { .string($0) })
            ])
        case "unmount":
            guard let childId = payload["child_id"].asString, !childId.isEmpty else {
                return .object(["error": .string("web.unmount: child_id required")])
            }
            guard let server = serverFor(agent.id) else {
                return .object(["error": .string("web.unmount: not running")])
            }
            server.unmountRoutes(for: AgentId(childId))
            return .object(["unmounted": .string(childId)])
        default:
            // python `{"error": f"web: unknown type {t!r}"}` — a plain error dict,
            // not a coded refusal (web's handler predates the refusal table).
            return .object(["error": .string("web: unknown type \(pyRepr(type))")])
        }
    }

    // MARK: - verbs

    /// python `_reflect` — identity + port + running flag + mounted surfaces.
    private func reflect(agent: Agent, kernel: Kernel) -> JSON {
        let server = serverFor(agent.id)
        var surfaces: OrderedDictionary<String, JSON> = [:]
        for (child, paths) in server?.routesByChild() ?? [:] {
            surfaces[child.value] = .array(paths.map { .string($0) })
        }
        let portVal = agent.metaValue(forKey: "port") ?? .null
        var out: OrderedDictionary<String, JSON> = [
            "id": .string(agent.id.value),
            "sentence": .string(
                "HTTP host — rendering routes baked in; call surfaces mounted from sub-agents."),
            "port": pyTruthy(portVal) ? .integer(Int64((try? pyInt(portVal)) ?? 0)) : .null,
            "running": .bool(server != nil),
            "served_agent_count": .integer(Int64(kernel.allAgents().count)),
            "surfaces": .object(surfaces),
            // THE BROWSER-FACING CONTRACT. `/file/` is the only route a browser
            // hits, and it was described NOWHERE. Measured 2026-08-21: a model
            // reflected this agent, found no route contract, and HALLUCINATED
            // one — inventing a `routes` key and the rule "the agent must be in
            // can_call AND expose read_stream", which omits the ingress half —
            // then verified against its own fiction.
            "routes": .object([
                "GET /<agent_id>/file/<path>": .string(
                    "Streams a file by read_stream-ing through the named agent. The "
                        + "read is dispatched AS THIS WEB AGENT, so it needs BOTH sides of "
                        + "the call gate — this host's can_call[<agent_id>] (egress) AND that "
                        + "agent's callable_by[<this web id>] (ingress) — plus that agent's "
                        + "own io leg open (ingress_rule). Any one missing → 404. See "
                        + "browser_reachable for who actually passes all three.")
            ]),
            // The PROPERTY, computed — not a description of it. `ingress_rule=
            // allow_all_ingress` reads like "open" while the call gate still
            // refuses; this answers "what can a browser actually get?".
            "browser_reachable": .array(
                browserReachable(webId: agent.id, kernel: kernel).map { .string($0) }),
        ]
        out["verbs"] = .object(Self.verbDocs)
        return .object(out)
    }

    /// python `_boot` — build the host on `rec.port`, mount every child's
    /// surface, THEN start serving, so the port never answers a request before
    /// its route table is in place.
    private func boot(agent: Agent, kernel: Kernel) async throws -> JSON {
        var server = serverFor(agent.id)
        if server == nil {
            // python `_spawn`: `rec.port` is REQUIRED, with no default — a
            // missing or zero port raises out of the verb.
            let portVal = agent.metaValue(forKey: "port") ?? .null
            guard pyTruthy(portVal) else {
                throw PyError(
                    "RuntimeError",
                    "webapp \(agent.id.value): rec.port is required (no default). "
                        + "Set port on create_agent or via update_agent.")
            }
            let port = try pyInt(portVal)
            let built = WebServer(
                kernel: kernel, agentId: agent.id, port: UInt16(truncatingIfNeeded: port))
            setServer(built, for: agent.id)
            server = built
        }
        guard let handle = server else { return .object(["running": .bool(false)]) }
        // Mount all child surfaces BEFORE the listener starts accepting.
        await mountAllSurfaces(server: handle, webId: agent.id, kernel: kernel)
        do {
            try handle.start()
        } catch {
            // python's serve loop runs as a task: a bind failure logs and pops
            // the handle, while `_boot` has already returned `running: True`.
            FileHandle.standardError.write(
                "webapp \(agent.id.value) :\(handle.port) serve failed: \(error)\n"
                    .data(using: .utf8) ?? Data())
            setServer(nil, for: agent.id)
        }
        // Announce OURSELVES to the cli renderer (the boot-event convention):
        // the web host owns its port, so it tells the terminal where to attach —
        // the renderer never reaches in for it. Best-effort: cli's `say` returns
        // None; if there is no cli (non-tty), fall back to a stderr log so
        // container logs still record the bind.
        let line =
            "listening on http://127.0.0.1:\(handle.port)/"
            + " · rest/ws are children (reflect kernel for ids)"
        let reply = await kernel.send([
            Item(
                AgentId("cli"),
                .object([
                    "type": .string("say"), "source": .string("web"), "text": .string(line),
                ]))
        ])[0].result
        if !reply.isNull {
            FileHandle.standardError.write(
                "  [web] \(agent.id.value) listening on http://localhost:\(handle.port)/\n"
                    .data(using: .utf8) ?? Data())
        }
        return .object([
            "running": .bool(true),
            "surfaces": .array(handle.routesByChild().keys.map { .string($0.value) }),
        ])
    }

    /// python `_browser_reachable` — which agents a browser could actually
    /// fetch through this host, right now. Asks the REAL gates rather than
    /// re-deriving them: `authorizeCall` both ways plus the io leg's own
    /// resolved ingress rule.
    private func browserReachable(webId: AgentId, kernel: Kernel) -> [String] {
        guard let web = kernel.agent(webId) else { return [] }
        var out: [String] = []
        for agent in kernel.allAgents() where agent.id != webId {
            let meta = agent.meta
            // An agent that cannot answer the SOURCE verb is not a file server.
            let verbs = meta["verbs"] ?? .null
            let named: Bool
            switch verbs {
            case .object(let o): named = o.keys.contains("read_stream")
            case .array(let a): named = a.contains { $0.asString == "read_stream" }
            default: named = false
            }
            // `handler_module` is a structural field on the agent, not meta —
            // python reads it off the record, where both live.
            let servable = named || (agent.handlerModule ?? "").hasPrefix("file_bridge")
            if !servable { continue }
            let egress = authorizeCall(
                grant: web.metaValue(forKey: "can_call") ?? .null, verb: "read_stream",
                otherId: agent.id.value, holderId: webId.value, parentId: web.parentId?.value,
                childIds: Set(web.childIds().map { $0.value }), loaderId: nil)
            if !egress.allowed { continue }
            let ingress = authorizeCall(
                grant: meta["callable_by"] ?? .null, verb: "read_stream", otherId: webId.value,
                holderId: agent.id.value, parentId: agent.parentId?.value,
                childIds: Set(agent.childIds().map { $0.value }), loaderId: nil)
            if !ingress.allowed { continue }
            guard
                let rule = try? resolveIngress(
                    ingressRule: meta["ingress_rule"], auth: meta["auth"])
            else { continue }
            let action = AuthAction(
                kind: "call", target: agent.id.value, verb: "read_stream", token: nil)
            if case .deny = gateInbound(rule: rule, action: action) { continue }
            out.append(agent.id.value)
        }
        return out.sorted()
    }

    /// python's `VERBS` table, first doc line each.
    static let verbDocs: OrderedDictionary<String, JSON> = [
        "reflect": .string(
            "Identity + uvicorn port + running flag + currently mounted surfaces (process-local; reflect the live serve for truth)."
        ),
        "boot": .string(
            "Idempotent. Builds the host on rec.port (required, no default), mounts each"),
        "stop": .string(
            "No args. Asks uvicorn to shut down; cancels its task. Returns {stopped:bool}."),
        "mount": .string(
            "args: child_id:str (req). Pulls `get_routes` from the named child and (re)mounts its routes onto the live app. Idempotent — unmounts any prior routes for the child first."
        ),
        "unmount": .string(
            "args: child_id:str (req). Removes every route belonging to the named child from the live app."
        ),
    ]

    /// python `PB/web/host/src/web/readme.md`, byte for byte.
    static let readmeText = """
        # web — uvicorn HTTP host
        Serves rendering only: `/` (tree), `/<id>/file/<path>` (static file proxy), favicon. No server-side `/<id>/` render route — frontend panels live in the TS kernel. Verb-invocation surfaces are sub-agents (web_ws, web_rest): they DECLARE routes via the duck-typed `get_routes` verb (pure data — `{kind, method?, path}`) and hand the live endpoint for each path to this host through the duck-typed `route_endpoints(agent)` module hook. A leg declaring a route without that hook fails web boot. `port` field on the record is where it binds.

        """
}
