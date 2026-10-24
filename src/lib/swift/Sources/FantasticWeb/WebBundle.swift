// HTTP host bundle (python `web.tools`).
//
// `web` owns the listener and renders nothing of its own: the baked-in
// routes are `/_assets/favicon.png` and `/<id>/file/<path>` (an undeclared
// `/` is a 404; all UI is an app on the TS kernel). Verb-invocation routes are NOT —
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
//   fetch_file -> one `read_stream` chunk of an agent's file, read AS THIS HOST —
//                the file route hands each browser fetch here
//
// IDENTITY: the HTTP server is the TRANSPORT — its request tasks run outside
// every dispatch, so a request is the operator. The file route never reads as
// anyone itself: it hands the fetch to this host (`fetch_file`), and the host
// reads as itself, gated as `web`.

import FantasticIoBridge
import FantasticJSON
import FantasticKernel
import Foundation
import OrderedCollections

public let HANDLER_MODULE = "web.tools"

public final class WebBundle: AgentBundle, @unchecked Sendable {
    public let name = "web"
    public var verbs: [String] { Array(Self.verbDocs.keys) }
    public var verbDocTable: JSON { .object(Self.verbDocs) }
    public init() {}

    /// Live `WebServer` instances keyed by agent id — python's `_servers`.
    /// Created in `boot`, dropped by `stop` and by the `onShutdown`/`onDelete`
    /// hooks. NSLock-protected because boot/stop can race with concurrent verb
    /// dispatch.
    private let serversLock = NSLock()
    private var servers: [AgentId: WebServer] = [:]

    /// The live server of web agent `id` (internal: a test reads its route table).
    func serverFor(_ id: AgentId) -> WebServer? {
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
    /// `_mount_all_surfaces`. A broken leg fails the walk (and so the boot).
    private func mountAllSurfaces(server: WebServer, webId: AgentId, kernel: Kernel) async throws {
        guard let web = kernel.agent(webId) else { return }
        for child in web.childIds() {
            try await mountSurface(server: server, childId: child, kernel: kernel)
        }
    }

    /// Mount one child's surface — python `_mount_surface`. Two halves:
    ///
    /// - the DECLARATION — the `get_routes` reply, pure JSON data, so it renders
    ///   over REST and persists into the ledger like any other reply;
    /// - the LIVE endpoints — the child bundle's `routeEndpoints(agent:kernel:)`
    ///   hook (python `route_endpoints(agent)`), keyed by the declared path.
    ///
    /// A child that answers no routes has no surface (no-op). A child that
    /// DECLARES routes but exposes no hook — or no endpoint for a declared path —
    /// is a broken leg: fail loud (python's RuntimeError), never mount half a
    /// surface. Every declared route is checked before any of them is mounted.
    private func mountSurface(server: WebServer, childId: AgentId, kernel: Kernel) async throws {
        let reply = await kernel.send([Item(childId, .object(["type": .string("get_routes")]))])[0]
            .result
        guard case .object = reply else { return }
        let routes = pyTruthy(reply["routes"]) ? (reply["routes"].asArray ?? []) : []
        if routes.isEmpty { return }
        guard let child = kernel.agent(childId), let hm = child.handlerModule,
            let hook = kernel.bundles.get(hm) as? WebRouteEndpoints
        else {
            throw PyError(
                "RuntimeError",
                "web: \(childId.value) declares \(routes.count) route(s) but its bundle "
                    + "exposes no route_endpoints(agent) hook")
        }
        let endpoints = hook.routeEndpoints(agent: child, kernel: kernel)
        var specs: [RouteSpec] = []
        for r in routes {
            // python keys the mount off `kind`: `websocket` mounts a websocket
            // route, anything else an HTTP route whose method defaults to GET.
            let kind: RouteKind = r["kind"].asString == "websocket" ? .websocket : .http
            // An endpoint of the other kind is no endpoint for this route: a
            // websocket declaration cannot be served by an HTTP callable.
            let path = r["path"].asString ?? ""
            let endpoint = endpoints[path]
            let matches: Bool
            switch (kind, endpoint) {
            case (.websocket, .websocket?), (.http, .http?): matches = true
            default: matches = false
            }
            guard !path.isEmpty, matches, let endpoint else {
                throw PyError(
                    "RuntimeError",
                    "web: \(childId.value) declares route \(pyRepr(r["path"])) but "
                        + "route_endpoints() hands over no endpoint for it")
            }
            let method = (pyTruthy(r["method"]) ? pyStr(r["method"]) : "GET").uppercased()
            specs.append(
                RouteSpec(
                    kind: kind, method: method, path: path, ownerId: childId, endpoint: endpoint))
        }
        server.mountRoutes(specs)
    }

    public var readme: String? { Self.readmeText }

    /// `fetch_file` answers with the chunk RAW, so it rides the binary channel;
    /// every other verb answers the header as the text channel would.
    public func handleBinary(
        agentId: AgentId, header: JSON, body: Blob?, kernel: Kernel
    ) async throws -> (JSON?, Blob?) {
        if header["type"].asString == "fetch_file" {
            return await fetchFile(header, kernel: kernel, raw: true)
        }
        return (try await handle(agentId: agentId, payload: header, kernel: kernel), nil)
    }

    /// python `_fetch_file` — one `read_stream` chunk of `path` on `agent_id`,
    /// read AS THIS HOST (this handler's own send), so the served agent's gates
    /// judge `web`. The chunk comes back raw on the binary channel.
    private func fetchFile(_ payload: JSON, kernel: Kernel, raw: Bool) async -> (JSON?, Blob?) {
        guard let agentId = payload["agent_id"].asString, !agentId.isEmpty else {
            return (.object(["error": .string("web.fetch_file: agent_id required")]), nil)
        }
        let out = await kernel.send([
            Item(
                AgentId(agentId),
                .object([
                    "type": .string("read_stream"),
                    "path": payload.asObject?["path"] ?? .null,
                    "offset": payload.asObject?["offset"] ?? .null,
                    "length": payload.asObject?["length"] ?? .null,
                ]), rawReply: raw)
        ])[0]
        return (out.result, out.body)
    }

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
            try await mountSurface(server: server, childId: AgentId(childId), kernel: kernel)
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
        case "fetch_file":
            return await fetchFile(payload, kernel: kernel, raw: false).0
        default:
            // python `refusal(UNHANDLED, f"web: unknown type {t!r}", verb=t)`.
            return Refusal.make(
                Refusal.unhandled, "web: unknown type \(pyRepr(type))",
                extra: [("verb", payload["type"])])
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
        let rec: JSON = .object([
            "host": agent.metaValue(forKey: "host") ?? .null, "port": portVal,
            "allowed_origins": agent.metaValue(forKey: "allowed_origins") ?? .null,
        ])
        var out: OrderedDictionary<String, JSON> = [
            "id": .string(agent.id.value),
            "sentence": .string(
                "HTTP host — rendering routes baked in; call surfaces mounted from sub-agents."),
            "port": pyTruthy(portVal) ? .integer(Int64((try? pyInt(portVal)) ?? 0)) : .null,
            "host": .string(webHost(rec)),
            "allowed_origins": .array(
                (webOwnOrigins(rec).sorted()
                    + webExtraOrigins(rec).filter { !webOwnOrigins(rec).contains($0) })
                    .map { .string($0) }),
            "running": .bool(server != nil),
            "served_agent_count": .integer(Int64(kernel.allAgents().count)),
            "surfaces": .object(surfaces),
            // THE BROWSER-FACING CONTRACT. `/file/` is the only route a browser
            // hits, and a model that finds no route contract on reflect
            // invents one.
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
            let built = WebServer(kernel: kernel, agentId: agent.id, port: port)
            setServer(built, for: agent.id)
            server = built
        }
        guard let handle = server else { return .object(["running": .bool(false)]) }
        // Mount all child surfaces BEFORE the listener starts accepting. A
        // broken leg raises out of `boot` (python's RuntimeError).
        try await mountAllSurfaces(server: handle, webId: agent.id, kernel: kernel)
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
        // the renderer never reaches in for it. No cli (non-tty) ⇒ nobody is
        // told; the bound port is on this agent's reflect. The URL names the
        // BOUND host — a host bound to a LAN address does not answer at
        // 127.0.0.1.
        let line =
            "listening on http://\(handle.host):\(handle.port)/"
            + " · rest/ws are children (reflect kernel for ids)"
        _ = await kernel.send([
            Item(
                AgentId("cli"),
                .object([
                    "type": .string("say"), "source": .string("web"), "text": .string(line),
                ]))
        ])
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
                ancestorIds: kernel.ancestorIds(of: agent))
            if !egress.allowed { continue }
            let ingress = authorizeCall(
                grant: meta["callable_by"] ?? .null, verb: "read_stream", otherId: webId.value,
                holderId: agent.id.value, parentId: agent.parentId?.value,
                ancestorIds: kernel.ancestorIds(of: web))
            if !ingress.allowed { continue }
            let rule = resolveIngress(ingressRule: meta["ingress_rule"], auth: meta["auth"])
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
        "fetch_file": .string(
            "args: agent_id:str (req), path:str (req), offset:int (req), length:int (req). One `read_stream` chunk of `path` on `agent_id`, read AS THIS HOST — the file route hands every browser fetch here, so the served agent's gates judge `web`. Returns that agent's reply."
        ),
    ]

    /// python `PB/web/host/src/web/readme.md`, byte for byte.
    static let readmeText = """
        # web — uvicorn HTTP host
        Renders nothing of its own (an undeclared `/` is a 404): it serves `/<id>/file/<path>` (the `read_stream` file proxy) and the favicon (`/_assets/favicon.png`). Frontend html agents live in an app on the TS kernel. Verb-invocation surfaces are sub-agents (web_ws, web_rest): they DECLARE routes via the duck-typed `get_routes` verb (pure data — `{kind, method?, path}`) and hand the live endpoint for each path to this host through the duck-typed `route_endpoints(agent)` module hook. A leg declaring a route without that hook fails web boot. `port` field on the record is where it binds; `bind` is the address the listener binds, else `host`, else `127.0.0.1` (loopback); `host` is the address the guard judges by. Set `bind: 0.0.0.0` to publish (a container): the listener takes every address and the guard keeps its loopback identity, so `localhost:<port>` is admitted. EVERY request — GET/HEAD/OPTIONS, unsafe methods and the websocket upgrade — whose `Host` header is not this host's own authority (`<host>:<port>`, plus `localhost`/`127.0.0.1`/`[::1]` on that port when the bind is loopback) is refused — 403 / refused upgrade, logged; a `host` of `0.0.0.0` answers only `Host: 0.0.0.0:<port>`, so a LAN name or a tunnel name is refused, and a `bind` alone never changes the guard's answers (a DNS-rebinding page names its own domain in Host). Websocket upgrades and non-GET/HEAD/OPTIONS requests from a browser `Origin` that is not this host's own (`http://<host>:<port>`, both loopback spellings when loopback) nor listed in the record's `allowed_origins` (exact strings) are refused — 403 / refused upgrade, logged. No Origin header (CLI, curl, a bridge) passes; the leg's `ingress_rule` still gates it. Both fields are ordinary record meta: create_agent, a roster yaml, update_agent.

        """
}
