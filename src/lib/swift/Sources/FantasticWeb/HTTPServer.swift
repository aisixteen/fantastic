// HTTP server backed by swift-nio (cross-platform: macOS + Linux).
//
// Routes (python `bundled_agents/web/host/src/web/app.py` — the reference):
//   GET  /                          → the agent-tree index (templates/index.html)
//   GET  /_assets/favicon.png       → the bundled favicon
//   GET  /favicon.png               → the bundled favicon
//   GET  /{agent_id}/file/{path}    → proxy to the agent's `read_stream` SOURCE
//   <method> /<leg>/<...>           → routes a child leg declared via `get_routes`
//   GET  /{host_id}/ws              → WebSocket upgrade → the shared proxy
//
// Matching is Starlette's over the ORDERED route list: first path+method match
// handles; else the first path-only match answers 405 with `allow`; else (path
// ≠ `/`) the trailing slash is toggled and a match answers 307; else 404
// `{"detail":"Not Found"}`. No transport.js, no vendored asset tree, no
// server-side `/<id>/` render route — those were swift-only extras (D11).

import FantasticIoBridge
import FantasticJSON
import FantasticKernel
import Foundation
import NIOCore
import NIOFoundationCompat
import NIOHTTP1
import NIOPosix
import NIOWebSocket
import OrderedCollections

/// Surface kind a child agent's route contributes. Mirrors the
/// `kind` field of Python's `get_routes` descriptors.
public enum RouteKind: String, Sendable {
    case websocket
    case http
}

/// One template segment. `{name}` captures one non-empty segment
/// (starlette's `[^/]+`); `{name:path}` swallows the rest (`.*`).
enum PathSegment: Sendable, Equatable {
    case literal(String)
    case param(String)
    case pathParam(String)
}

/// A route contributed by a child agent (web_ws / web_rest), pulled
/// at host boot via the child's `get_routes` verb. `kind` selects the
/// host's shared handler: `.websocket` runs the shared WS proxy;
/// `.http` calls the owning bundle's `WebRouteOwner.serveRoute`.
public struct RouteSpec: Sendable {
    public let kind: RouteKind
    public let method: String  // uppercased; "GET" for websocket upgrades
    public let path: String
    let template: [PathSegment]
    public let ownerId: AgentId

    public init(kind: RouteKind, method: String, path: String, ownerId: AgentId) {
        self.kind = kind
        self.method = method.uppercased()
        self.path = path
        self.template = parseTemplate(path)
        self.ownerId = ownerId
    }

    /// Match request `segments` (raw, empty ones KEPT) against this template.
    func match(segments: [String]) -> [String: String]? {
        matchTemplate(template, segments)
    }
}

/// Split a path into raw segments, keeping empty ones — `/a/b/` is
/// `["", "a", "b", ""]`, so a trailing slash is a real difference (starlette
/// matches the whole path string, it does not fold separators).
func rawSegments(_ path: String) -> [String] {
    path.split(separator: "/", omittingEmptySubsequences: false).map(String.init)
}

/// uvicorn's ASGI `path` (`h11_impl.py:201`): the raw target MINUS its query,
/// percent-decoded ONCE. Routing, the route params and the file proxy all read
/// that decoded string, so `POST /rest/fi%6Ces` names the `files` agent and
/// `GET /files/file/sp%20ace.txt?v=1` names `sp ace.txt` (A13.10). Swift matched
/// the RAW target, so both answered 404 and a query string became part of the
/// filename.
///
/// `removingPercentEncoding` is `nil` only for a byte sequence that is not
/// UTF-8; python's `unquote` replaces those with U+FFFD rather than failing, and
/// keeping the raw text here is the closest non-lossy stand-in.
func decodePath(_ target: String) -> String {
    let pathOnly = splitQuery(target).0
    return pathOnly.removingPercentEncoding ?? pathOnly
}

func parseTemplate(_ path: String) -> [PathSegment] {
    rawSegments(path).map { s in
        guard s.hasPrefix("{"), s.hasSuffix("}") else { return .literal(s) }
        let inner = String(s.dropFirst().dropLast())
        if let colon = inner.firstIndex(of: ":"), inner[inner.index(after: colon)...] == "path" {
            return .pathParam(String(inner[..<colon]))
        }
        return .param(inner)
    }
}

func matchTemplate(_ template: [PathSegment], _ segments: [String]) -> [String: String]? {
    var params: [String: String] = [:]
    var i = 0
    while i < template.count {
        switch template[i] {
        case .pathParam(let name):
            // `.*` — swallows the rest, may be empty, may contain `/`.
            guard i < segments.count else { return nil }
            params[name] = segments[i...].joined(separator: "/")
            return params
        case .param(let name):
            guard i < segments.count, !segments[i].isEmpty else { return nil }
            params[name] = segments[i]
        case .literal(let lit):
            guard i < segments.count, segments[i] == lit else { return nil }
        }
        i += 1
    }
    return i == segments.count ? params : nil
}

// MARK: - host bind + Origin guard (python `web/app.py` — 2026-09-24)

/// The web host binds LOOPBACK unless its record names a `host` — a closed default,
/// overridable by whoever creates the kernel (create_agent meta, a roster yaml,
/// update_agent). `0.0.0.0` is an explicit choice, never a fallback.
public let WEB_DEFAULT_HOST = "127.0.0.1"

let webSafeMethods: Set<String> = ["GET", "HEAD", "OPTIONS"]

/// python `rec.get("host") or DEFAULT_HOST`.
public func webHost(_ rec: JSON) -> String {
    let v = rec["host"]
    return pyTruthy(v) ? (v.asString ?? pyStr(v)) : WEB_DEFAULT_HOST
}

/// python `own_origins(rec)`: `http://<host>:<port>`, plus both loopback spellings
/// when it binds loopback. Never derived from the request's Host header (a
/// DNS-rebinding page controls it).
public func webOwnOrigins(_ rec: JSON) -> Set<String> {
    let host = webHost(rec)
    let port = pyStr(rec["port"])
    var out: Set<String> = ["http://\(host):\(port)"]
    if ["127.0.0.1", "localhost", "::1"].contains(host) {
        out.formUnion([
            "http://127.0.0.1:\(port)", "http://localhost:\(port)", "http://[::1]:\(port)",
        ])
    }
    return out
}

/// The record's `allowed_origins` as a list — a bare string counts as one item
/// (`k=v` on the CLI yields a string).
func webExtraOrigins(_ rec: JSON) -> [String] {
    let v = rec["allowed_origins"]
    if case .string(let s) = v { return s.isEmpty ? [] : [s] }
    return v.asArray?.compactMap { $0.asString } ?? []
}

/// python `origin_allowed(rec, origin)`: no Origin ⇒ a non-browser client, allowed
/// (the leg's `ingress_rule` still gates it); else own or listed, exact strings.
public func webOriginAllowed(_ rec: JSON, _ origin: String?) -> Bool {
    guard let origin = origin, !origin.isEmpty else { return true }
    return webOwnOrigins(rec).contains(origin) || webExtraOrigins(rec).contains(origin)
}

/// What a matched route runs.
enum RouteTarget: Sendable {
    case index
    case favicon
    case fileProxy
    case leg(AgentId)
}

/// One entry of the ordered route table.
struct Route: Sendable {
    let kind: RouteKind
    /// `nil` = every method (starlette `Route(methods=None)`); otherwise the
    /// declared set. FastAPI's `APIRoute` adds no implicit HEAD.
    let methods: Set<String>?
    let template: [PathSegment]
    let target: RouteTarget
}

/// Server lifecycle owner. Started by `WebBundle.boot`, stopped by
/// `WebBundle.stop`. One instance per `web.tools` agent.
public final class WebServer: @unchecked Sendable {
    let kernel: Kernel
    private let agentId: AgentId
    /// The port `rec.port` named — python binds exactly this, never a hint.
    public let port: UInt16
    private let lock = NSLock()
    private var channel: Channel?
    /// The task the `boot` verb ran under. Every request runs as its CHILD, so
    /// a REST-originated dispatch has the web boot pid as its parent (python
    /// serves under `CURRENT_TASK.scope`).

    /// Process-shared event loop group — never shut down (mirrors the
    /// kernel_bridge's static-shared convention + NIO best practice).
    static let group: EventLoopGroup = MultiThreadedEventLoopGroup.singleton

    /// Dynamic routes contributed by child surface agents (web_ws /
    /// web_rest), pulled at host boot via each child's `get_routes`, in mount
    /// order. The host serves only the four python rendering routes natively.
    private var dynamicRoutes: [RouteSpec] = []

    public init(kernel: Kernel, agentId: AgentId, port: UInt16) {
        self.kernel = kernel
        self.agentId = agentId
        self.port = port
    }

    /// Mount `specs` as the owner's routes — python's
    /// `routes_by_child[child_id] = mounted`, so a re-boot or a re-mount
    /// REPLACES what that child owns instead of stacking duplicates.
    public func mountRoutes(_ specs: [RouteSpec]) {
        guard let owner = specs.first?.ownerId else { return }
        lock.lock()
        defer { lock.unlock() }
        dynamicRoutes.removeAll { $0.ownerId == owner }
        dynamicRoutes.append(contentsOf: specs)
    }

    /// Drop every route owned by `owner`.
    public func unmountRoutes(for owner: AgentId) {
        lock.lock()
        defer { lock.unlock() }
        dynamicRoutes.removeAll { $0.ownerId == owner }
    }

    /// python's `handle.routes_by_child` — child id → the paths it owns, in
    /// mount order.
    public func routesByChild() -> OrderedDictionary<AgentId, [String]> {
        lock.lock()
        let routes = dynamicRoutes
        lock.unlock()
        var out: OrderedDictionary<AgentId, [String]> = [:]
        for r in routes { out[r.ownerId, default: []].append(r.path) }
        return out
    }

    public func paths(for owner: AgentId) -> [String] {
        lock.lock()
        defer { lock.unlock() }
        return dynamicRoutes.filter { $0.ownerId == owner }.map { $0.path }
    }

    func matchWebSocket(segments: [String]) -> (RouteSpec, [String: String])? {
        lock.lock()
        let routes = dynamicRoutes
        lock.unlock()
        for spec in routes where spec.kind == .websocket {
            if let params = spec.match(segments: segments) { return (spec, params) }
        }
        return nil
    }

    /// The ORDERED route table a request walks: python's four baked routes
    /// first (FastAPI appends user routes after its own `setup()` ones), then
    /// each mounted child's declared routes in order.
    func routeTable() -> [Route] {
        lock.lock()
        let dynamic = dynamicRoutes
        lock.unlock()
        var out: [Route] = [
            Route(
                kind: .http, methods: ["GET"], template: parseTemplate("/"), target: .index),
            Route(
                kind: .http, methods: ["GET"], template: parseTemplate("/_assets/favicon.png"),
                target: .favicon),
            Route(
                kind: .http, methods: ["GET"], template: parseTemplate("/favicon.png"),
                target: .favicon),
            Route(
                kind: .http, methods: ["GET"],
                template: parseTemplate("/{agent_id}/file/{path:path}"), target: .fileProxy),
        ]
        for spec in dynamic {
            out.append(
                Route(
                    kind: spec.kind, methods: [spec.method], template: spec.template,
                    target: .leg(spec.ownerId)))
        }
        return out
    }

    /// Start the listener on exactly `port`. Synchronous (binds + waits) so
    /// `WebBundle.boot` sees the bind failure it must log. IDEMPOTENT, like
    /// python's `_start_serving` (`if handle.task is not None: return`) — `boot`
    /// is idempotent and must not re-bind a port it already holds.
    public func start() throws {
        lock.lock()
        let alreadyServing = channel != nil
        lock.unlock()
        if alreadyServing { return }
        let bootstrap = ServerBootstrap(group: WebServer.group)
            .serverChannelOption(ChannelOptions.backlog, value: 256)
            .serverChannelOption(ChannelOptions.socketOption(.so_reuseaddr), value: 1)
            .childChannelOption(ChannelOptions.socketOption(.so_reuseaddr), value: 1)
            .childChannelInitializer { [weak self] channel in
                guard let self = self else {
                    return channel.eventLoop.makeSucceededVoidFuture()
                }
                return self.configurePipeline(channel)
            }
        // `.wait()` blocks the calling thread (WebBundle.boot's task thread,
        // never an event-loop thread) until the bind resolves or throws.
        // LOOPBACK unless the record names a host (`host: "0.0.0.0"` to publish).
        let bound = try bootstrap.bind(host: webHost(record()), port: Int(port)).wait()
        lock.lock()
        channel = bound
        lock.unlock()
        kernel.setHttpPort(port)
    }

    public func stop() {
        lock.lock()
        let ch = channel
        channel = nil
        lock.unlock()
        // DRAINS, like python's `_stop_uvicorn`: it sets `should_exit` and then
        // `await`s the serve task, so when the `stop` verb answers the port no
        // longer accepts. A fire-and-forget `close(promise: nil)` only QUEUES
        // the close on an event loop, and the next `boot` then mounted its
        // child surfaces against a socket that was still accepting — the
        // listener the caller was told had stopped (A11.30, flaky).
        // `.wait()` blocks the calling thread, never an event-loop thread, for
        // the same reason `start()`'s bind may block it.
        try? ch?.close().wait()
        kernel.setHttpPort(0)
    }

    /// The live record's guard-relevant fields, read PER REQUEST so an
    /// `allowed_origins` edit applies without a restart.
    func record() -> JSON {
        guard let a = kernel.agent(agentId) else { return .object([:]) }
        return .object([
            "host": a.metaValue(forKey: "host") ?? .null,
            "port": a.metaValue(forKey: "port") ?? .null,
            "allowed_origins": a.metaValue(forKey: "allowed_origins") ?? .null,
        ])
    }

    /// python `_OriginGuard`: a websocket upgrade, or an HTTP request that is not
    /// GET/HEAD/OPTIONS, from a FOREIGN browser origin is refused. `true` = refused.
    func originRefused(websocket: Bool, method: String, origin: String?) -> Bool {
        let gated = websocket || !webSafeMethods.contains(method)
        return gated && !webOriginAllowed(record(), origin)
    }

    // MARK: - Pipeline

    /// Configure a child connection's pipeline: HTTP server + a WebSocket
    /// upgrade path (the upgrader matches a `.websocket` route and, on
    /// upgrade, installs the shared `WebSocketProxyHandler`).
    private func configurePipeline(_ channel: Channel) -> EventLoopFuture<Void> {
        let kernel = self.kernel
        let upgrader = NIOWebSocketServerUpgrader(
            maxFrameSize: 1 << 24,  // 16 MiB: room for raw read_stream/write_stream chunks
            shouldUpgrade: { [weak self] channel, head in
                guard let self = self else {
                    return channel.eventLoop.makeSucceededFuture(nil)
                }
                // A foreign origin never upgrades: the request falls through to
                // the HTTP dispatch, which logs it and answers the 403.
                let origin = head.headers["origin"].first.flatMap { $0.isEmpty ? nil : $0 }
                if self.originRefused(websocket: true, method: "GET", origin: origin) {
                    return channel.eventLoop.makeSucceededFuture(nil)
                }
                let segs = rawSegments(decodePath(head.uri))
                if let (_, params) = self.matchWebSocket(segments: segs),
                    params["host_id"] != nil
                {
                    // uvicorn's 101 also carries `date` + `server` (python
                    // additionally negotiates permessage-deflate, which needs a
                    // compression implementation — recorded, not ported).
                    var extra = HTTPHeaders()
                    extra.add(name: "date", value: httpDate())
                    extra.add(name: "server", value: "uvicorn")
                    return channel.eventLoop.makeSucceededFuture(extra)
                }
                return channel.eventLoop.makeSucceededFuture(nil)
            },
            upgradePipelineHandler: { [weak self] channel, head in
                guard let self = self,
                    case let segs = rawSegments(decodePath(head.uri)),
                    let (spec, params) = self.matchWebSocket(segments: segs),
                    let hostId = params["host_id"]
                else {
                    return channel.eventLoop.makeSucceededVoidFuture()
                }
                return channel.pipeline.addHandler(
                    WebSocketProxyHandler(
                        hostId: hostId, legId: spec.ownerId, kernel: kernel,
                        // The same boot pid an HTTP request task is parented to:
                        // python serves BOTH under `CURRENT_TASK.scope` (A11.31).
                        bootParent: self.requestParent))
            })
        let httpHandler = HTTPDispatchHandler(server: self)
        let upgradeConfig: NIOHTTPServerUpgradeConfiguration = (
            upgraders: [upgrader],
            completionHandler: { _ in
                // Upgrade SUCCEEDED — the plain-HTTP dispatch handler is no
                // longer needed and must be removed, else post-upgrade
                // WebSocket bytes reach its HTTP decoder and crash ("found
                // IOData, expected HTTPPart"). NIO removes the HTTP codec it
                // installed, but NOT handlers we added after the upgrader.
                channel.pipeline.removeHandler(httpHandler, promise: nil)
            }
        )
        return channel.pipeline.configureHTTPServerPipeline(
            withServerUpgrade: upgradeConfig
        ).flatMap {
            channel.pipeline.addHandler(httpHandler)
        }
    }

    // MARK: - Request handling (HTTP; WS is handled by the upgrader above)

    /// NO parent: python clears `_current_task` for a REST task (2026-09-20) —
    /// the web boot dispatch has ended, so a pid parented to it was dangling.
    var requestParent: TaskId? { nil }

    func handle(request: HTTPRequest) async -> HTTPResponse {
        // THE QUERY IS STRIPPED AND THE PATH PERCENT-DECODED ONCE, BEFORE
        // MATCHING (A13.10) — uvicorn hands the app a decoded, query-less path.
        let (_, query) = splitQuery(request.path)
        let pathOnly = decodePath(request.path)
        // THE ORIGIN GUARD, in front of every route (python `_OriginGuard`).
        var origin: String?
        var websocket = false
        for (k, v) in request.headers {
            switch k.lowercased() {
            case "origin": origin = v.isEmpty ? nil : v
            case "upgrade": websocket = websocket || v.lowercased() == "websocket"
            default: break
            }
        }
        if originRefused(websocket: websocket, method: request.method, origin: origin) {
            let kind = websocket ? "websocket" : "http"
            let shown = pyRepr(origin.map { JSON.string($0) } ?? .null)
            FileHandle.standardOutput.write(
                Data(
                    "  [web] \(agentId.value): refused \(kind) \(pathOnly) from foreign origin \(shown)\n"
                        .utf8))
            // A websocket is refused on the handshake (python closes 1008 before
            // accept ⇒ uvicorn answers 403, no body).
            if websocket { return HTTPResponse(status: 403, contentType: nil, body: Data()) }
            let body: JSON = .object([
                "error": .string("origin \(shown) is not allowed on this host"),
                "reason": .string("unauthorized"),
                "hint": .string("add it to the web record's allowed_origins"),
            ])
            return HTTPResponse(
                status: 403, contentType: "application/json", body: Data(pyJsonDumps(body).utf8))
        }
        let segments = rawSegments(pathOnly)
        let table = routeTable()

        // Starlette `Router.app`: first FULL match wins; remember the first
        // path-only match for the 405.
        var partial: Route?
        for route in table where route.kind == .http {
            guard let params = matchTemplate(route.template, segments) else { continue }
            if route.methods == nil || route.methods!.contains(request.method) {
                return await serve(
                    route: route, params: params, query: query, pathOnly: pathOnly,
                    request: request)
            }
            if partial == nil { partial = route }
        }
        if let partial = partial {
            let allow = (partial.methods ?? []).sorted().joined(separator: ", ")
            return HTTPResponse(
                status: 405, contentType: "application/json",
                body: Data(JSON.object(["detail": .string("Method Not Allowed")]).serialize().utf8),
                extraHeaders: [("allow", allow)])
        }
        // `redirect_slashes`: toggle the trailing slash and, if ANY route
        // matches the toggled path (method ignored), answer 307.
        if pathOnly != "/" {
            let toggled =
                pathOnly.hasSuffix("/") ? String(pathOnly.dropLast()) : pathOnly + "/"
            let toggledSegments = rawSegments(toggled)
            if table.contains(where: {
                $0.kind == .http && matchTemplate($0.template, toggledSegments) != nil
            }) {
                var host = "127.0.0.1:\(port)"
                for (k, v) in request.headers where k.lowercased() == "host" { host = v }
                let q = request.path.firstIndex(of: "?").map { String(request.path[$0...]) } ?? ""
                return HTTPResponse(
                    status: 307, contentType: nil, body: Data(),
                    extraHeaders: [("location", "http://\(host)\(toggled)\(q)")])
            }
        }
        // FastAPI's 404 handler — `{"detail":"Not Found"}`, 22 bytes.
        return HTTPResponse(
            status: 404, contentType: "application/json",
            body: Data(JSON.object(["detail": .string("Not Found")]).serialize().utf8))
    }

    private func serve(
        route: Route, params: [String: String], query: [String: String], pathOnly: String,
        request: HTTPRequest
    ) async -> HTTPResponse {
        switch route.target {
        case .index:
            return await serveIndex()
        case .favicon:
            return HTTPResponse(status: 200, contentType: "image/png", body: Self.faviconBytes)
        case .fileProxy:
            return await serveFileProxy(
                agentId: params["agent_id"] ?? "", path: params["path"] ?? "")
        case .leg(let owner):
            return await dispatchLegRoute(
                owner: owner, params: params, query: query, pathOnly: pathOnly, request: request)
        }
    }

    /// python `_index_page` — the substrate tree rendered into
    /// `templates/index.html`.
    ///
    /// AS THE WEB AGENT, like every other route on this host. Python starts
    /// uvicorn INSIDE the `boot` dispatch, so the `_current_sender` ContextVar
    /// the request handlers inherit is already the web agent's id and the
    /// roster walk is gated per node. Swift's listener is a NIO channel with no
    /// task-local to inherit, so this send carried NO sender, i.e. OPERATOR
    /// authority — the walk reached a sibling host with no `callable_by` and
    /// that sibling's watcher saw an event python's browser never produces (one
    /// extra `wsx` frame in the acceptance capture).
    private func serveIndex() async -> HTTPResponse {
        let web = self.agentId
        let primer = await KernelTaskLocals.$currentSender.withValue(web) {
            await kernel.send([
                Item(AgentId("kernel"), .object(["type": .string("reflect")]))
            ])[0].result
        }
        let tree = primer["tree"]
        let body = pyTruthy(tree) ? Self.renderNode(tree) : "<li><em>empty tree</em></li>"
        let html = Self.indexTemplate.replacingOccurrences(of: "{{tree_body}}", with: body)
        return HTTPResponse(
            status: 200, contentType: "text/html; charset=utf-8", body: Data(html.utf8))
    }

    /// python `_index_page._render`.
    static func renderNode(_ node: JSON) -> String {
        let aid = text(node["id"])
        let nameVal = node["display_name"]
        let name = pyTruthy(nameVal) ? text(nameVal) : aid
        let hmVal = node["handler_module"]
        let hm = pyTruthy(hmVal) ? text(hmVal) : "(root)"
        let children = node["children"].asArray ?? []
        let kids =
            children.isEmpty
            ? "" : "<ul>" + children.map { renderNode($0) }.joined() + "</ul>"
        return "<li><span class=\"id\">\(esc(name))</span>"
            + " <code>\(esc(aid))</code>"
            + " <span class=\"hm\">\(esc(hm))</span>\(kids)</li>"
    }

    private static func text(_ v: JSON) -> String { v.asString ?? pyStr(v) }

    /// python `_esc`.
    static func esc(_ s: String) -> String {
        s.replacingOccurrences(of: "&", with: "&amp;")
            .replacingOccurrences(of: "<", with: "&lt;")
            .replacingOccurrences(of: ">", with: "&gt;")
    }

    /// python `agent_file` — pipe the agent's `read_stream` SOURCE.
    ///
    /// THROUGH THE BATCH, AS THE WEB AGENT (C41/C37). Python dispatches under
    /// `sender_context(web_agent_id)`, so the target's `callable_by` decides
    /// whether this route may read it at all; swift sent with NO sender, i.e. as
    /// the OPERATOR, and `sendWithBinary` left no execution behind either.
    private func serveFileProxy(agentId: String, path: String) async -> HTTPResponse {
        guard kernel.agent(AgentId(agentId)) != nil else { return Self.emptyNotFound }
        let web = self.agentId
        func readChunk(_ offset: Int) async -> (JSON, Data?) {
            let out = await KernelTaskLocals.$currentSender.withValue(web) {
                await kernel.send([
                    Item(
                        AgentId(agentId),
                        .object([
                            "type": .string("read_stream"), "path": .string(path),
                            "offset": .integer(Int64(offset)), "length": .integer(262144),
                        ]), binary: Data())
                ])[0]
            }
            return (out.result, out.body)
        }
        var (meta, body) = await readChunk(0)
        // THE SOURCE TEST IS THE CARRIER, python's `isinstance(first["bytes"],
        // bytes)`: anything that did not answer with a CHUNK is a 404 — a denied
        // gate, a path escape, a missing file, a non-server. An EMPTY chunk is
        // still a chunk.
        //
        // A GATE denial carries its reason in the body — this kernel teaches
        // through denials, and this route used to be the one surface that threw
        // that away. A MISSING FILE stays a bare 404: no body, no existence
        // leak, no path probing.
        guard var buf = body else {
            if meta["reason"].asString == "unauthorized" {
                // python `{k: detail.get(k) for k in (...)}` — all FOUR keys, a
                // key the denial lacks present as JSON null.
                var detail: OrderedDictionary<String, JSON> = [:]
                for k in ["error", "reason", "side", "hint"] { detail[k] = meta[k] }
                return HTTPResponse(
                    status: 404, contentType: "application/json",
                    body: Data(pyJsonDumps(.object(detail)).utf8))
            }
            return Self.emptyNotFound
        }
        // python reads `size` off the FIRST chunk and, when truthy, declares it
        // as `content-length`; a falsy size leaves the StreamingResponse
        // CHUNKED.
        let size = meta["size"].asInt ?? 0
        while !(meta["eof"].asBool ?? true) {
            let next = Int(meta["next_offset"].asInt ?? 0)
            (meta, body) = await readChunk(next)
            guard let chunk = body else { break }
            buf.append(chunk)
        }
        // python types the proxy with `mimetypes.guess_type(path)` and falls
        // back to octet-stream; starlette appends the charset to a `text/*`.
        var mime = Mimetypes.guessType(path) ?? "application/octet-stream"
        if mime.hasPrefix("text/") && !mime.contains("charset=") { mime += "; charset=utf-8" }
        return HTTPResponse(
            status: 200, contentType: mime, body: buf, chunked: size == 0)
    }

    /// python `Response(status_code=404)` — no body, no content-type.
    static let emptyNotFound = HTTPResponse(status: 404, contentType: nil, body: Data())

    // MARK: - leg routes

    static let octetType = "application/octet-stream"
    static let payloadHeader = "x-fantastic-payload"
    static let bytesFieldHeader = "x-fantastic-bytes-field"

    /// Serve a matched leg route. The JSON leg is the leg bundle's own
    /// `serveRoute` (python's mounted callable); the octet leg is the host's,
    /// because a JSON `{status, content_type, body}` reply cannot carry raw
    /// bytes or response headers.
    private func dispatchLegRoute(
        owner: AgentId, params: [String: String], query: [String: String], pathOnly: String,
        request: HTTPRequest
    ) async -> HTTPResponse {
        // Header values decode byte-for-byte as latin-1 (starlette's ASGI
        // layer), lowercased names.
        var lowerHeaders: [String: String] = [:]
        for (k, v) in request.headers { lowerHeaders[k.lowercased()] = latin1(v) }

        if request.method == "POST",
            (lowerHeaders["content-type"] ?? "").hasPrefix(Self.octetType)
        {
            return await dispatchOctetRoute(
                owner: owner, params: params, request: request, headers: lowerHeaders)
        }

        // THE BODY DECODES LIKE `json.loads(bytes)` (A13.11): CPython detects the
        // encoding from the BOM / the null-byte pattern of the first four bytes,
        // so a UTF-8-BOM, UTF-16 or UTF-32 body is ordinary JSON to python.
        // Bytes that will not decode raise `UnicodeDecodeError` OUT of the
        // endpoint (`web_rest/tools.py:117` catches only `JSONDecodeError`), so
        // starlette answers 500 text/plain — mirrored, not pinned.
        let bodyText: String
        switch Self.decodeJSONBody(request.body ?? Data()) {
        case .some(let t): bodyText = t
        case .none:
            return HTTPResponse(
                status: 500, contentType: "text/plain; charset=utf-8",
                body: Data("Internal Server Error".utf8))
        }

        var paramsObj: OrderedDictionary<String, JSON> = [:]
        for (k, v) in params.sorted(by: { $0.key < $1.key }) { paramsObj[k] = .string(v) }
        var queryObj: OrderedDictionary<String, JSON> = [:]
        for (k, v) in query.sorted(by: { $0.key < $1.key }) { queryObj[k] = .string(v) }
        var headerObj: OrderedDictionary<String, JSON> = [:]
        for (k, v) in lowerHeaders.sorted(by: { $0.key < $1.key }) { headerObj[k] = .string(v) }
        let routeRequest: JSON = .object([
            "method": .string(request.method),
            "path": .string(pathOnly),
            "params": .object(paramsObj),
            "query": .object(queryObj),
            "headers": .object(headerObj),
            "body": .string(bodyText),
        ])
        guard let agent = kernel.agent(owner), let hm = agent.handlerModule,
            let bundle = kernel.bundles.get(hm), let routeOwner = bundle as? WebRouteOwner
        else {
            return jsonHTTP(
                500, .object(["error": .string("no route owner \(owner.value)")]))
        }
        let (reply, chunk) = await routeOwner.serveRoute(
            legId: owner, request: routeRequest, kernel: kernel)
        let status = Int(reply["status"].asInt ?? 200)
        // ONE DECISION, shared with the octet leg: a reply that HOLDS a chunk is
        // octet, whatever its length (A13.1).
        if let chunk {
            return Self.octetReply(chunk, payloadHeader: reply["payload_header"].asString ?? "{}")
        }
        guard let contentType = reply["content_type"].asString else {
            return HTTPResponse(status: status, contentType: nil, body: Data())
        }
        return HTTPResponse(
            status: status, contentType: contentType,
            body: Data((reply["body"].asString ?? "").utf8))
    }

    /// python's octet response: the chunk as the body, the REST of the reply
    /// dict on `X-Fantastic-Payload` (python `json.dumps(rest)` — DEFAULT
    /// separators, not the compact ones the JSON body uses), and the field it
    /// came out of on `X-Fantastic-Bytes-Field`. An EMPTY chunk is still octet —
    /// content-length 0, not a JSON body.
    static func octetReply(_ chunk: Data, payloadHeader header: String) -> HTTPResponse {
        HTTPResponse(
            status: 200, contentType: octetType, body: chunk,
            extraHeaders: [(payloadHeader, header), (bytesFieldHeader, "bytes")])
    }

    /// The BINARY leg of a `.http` (web_rest) route — python's octet branch of
    /// `_rest_call`. The raw request body is one bytes-valued field; the rest of
    /// the envelope rides `X-Fantastic-Payload`, and the field is named by
    /// `X-Fantastic-Bytes-Field` (default `bytes`). Never base64.
    private func dispatchOctetRoute(
        owner: AgentId, params: [String: String],
        request: HTTPRequest, headers: [String: String]
    ) async -> HTTPResponse {
        guard let target = params["target_id"] else {
            return jsonHTTP(400, .object(["error": .string("web_rest: missing target")]))
        }
        // python `json.loads(headers.get(...) or "{}")` — an EMPTY header value
        // is an empty envelope, not a parse error.
        let raw = headers[Self.payloadHeader] ?? ""
        guard let parsed = try? JSON.parse(raw.isEmpty ? "{}" : raw) else {
            return jsonHTTP(
                400, .object(["error": .string("web_rest: X-Fantastic-Payload is not valid JSON")])
            )
        }
        guard case .object(var payload) = parsed else {
            return jsonHTTP(
                400,
                .object(["error": .string("web_rest: X-Fantastic-Payload must be a JSON object")]))
        }
        // python binds the body at `payload[field]`. Swift's JSON cannot hold
        // raw bytes, so the blob rides the binary channel — which the target
        // reads as its `bytes` field. Naming ANOTHER field therefore leaves the
        // header's own `bytes` in place and sends no blob, exactly as python's
        // target sees it.
        let field = headers[Self.bytesFieldHeader] ?? ""
        let blob: Data
        if field.isEmpty || field == "bytes" {
            payload.removeValue(forKey: "bytes")
            blob = request.body ?? Data()
        } else {
            blob = Data()
        }

        // GATE — same io_bridge inbound gate as the JSON path; credential on
        // X-Fantastic-Auth. The web_rest leg is the route's owner.
        if let denied = gateWebLeg(
            kernel: kernel, legId: owner, kind: "call", target: target,
            verb: payload["type"]?.asString ?? "", token: headers["x-fantastic-auth"])
        {
            return jsonHTTP(403, denied)
        }

        // THROUGH THE BATCH (A13.3/C37): ONE binary item, so the octet leg
        // NAMES its execution like the JSON leg — a repeated
        // `X-Fantastic-Execution-Id` on the same target REPLAYS the recorded
        // outcome instead of re-running the write, and the kernel produces
        // python's own refusals for a missing `payload.type` or a rebinding id.
        // `sendWithBinary` validated none of that: swift answered
        // `file_bridge: unknown type ""` where python answers
        // `send: payload.type required`, and stuffed the header's `id` into the
        // verb payload the target reads.
        var item = Item(AgentId(target), .object(payload), binary: blob)
        if let execId = headers["x-fantastic-execution-id"], !execId.isEmpty {
            item.id = execId
        }
        let out = await KernelTaskLocals.$currentSender.withValue(owner) {
            await kernel.send([item])[0]
        }
        // python answers 204 for a null reply BEFORE it looks for bytes.
        if out.result.isNull { return HTTPResponse(status: 204, contentType: nil, body: Data()) }
        // THE SAME ONE DECISION as the JSON leg: octet iff the verb returned a
        // CHUNK. `!outBody.isEmpty` served an empty file as JSON.
        if let chunk = out.body {
            return Self.octetReply(chunk, payloadHeader: pyJsonDumps(out.result))
        }
        return jsonHTTP(200, out.result)
    }

    /// CPython `json.detect_encoding` + `bytes.decode(enc, "surrogatepass")`
    /// (A13.11). `nil` = the bytes do not decode, which python raises on.
    ///
    /// RESIDUAL (D13): `surrogatepass` admits lone surrogates and CPython's
    /// scanner admits `NaN`/`Infinity`/`-Infinity`; `FantasticJSON` can
    /// represent neither, so those bodies still answer 400 where python answers
    /// 204. The ENCODING half below is exact.
    static func decodeJSONBody(_ b: Data) -> String? {
        if b.isEmpty { return "" }
        let n = b.count
        func at(_ i: Int) -> UInt8 { b[b.startIndex + i] }
        // BOMs first, widest first — `ff fe 00 00` is UTF-32-LE, and `ff fe`
        // alone is UTF-16-LE.
        // Foundation's decoders are LENIENT — they drop a trailing odd byte
        // instead of failing, which is the difference between python's 500 and a
        // silent 204 (measured `ff fe 7b`). The unit width is checked here.
        func wide(_ body: Data, _ enc: String.Encoding, _ unit: Int) -> String? {
            guard body.count % unit == 0 else { return nil }
            return String(data: body, encoding: enc)
        }
        if n >= 4, at(0) == 0x00, at(1) == 0x00, at(2) == 0xFE, at(3) == 0xFF {
            return wide(b.dropFirst(4), .utf32BigEndian, 4)
        }
        if n >= 4, at(0) == 0xFF, at(1) == 0xFE, at(2) == 0x00, at(3) == 0x00 {
            return wide(b.dropFirst(4), .utf32LittleEndian, 4)
        }
        if n >= 2, at(0) == 0xFE, at(1) == 0xFF {
            return wide(b.dropFirst(2), .utf16BigEndian, 2)
        }
        if n >= 2, at(0) == 0xFF, at(1) == 0xFE {
            return wide(b.dropFirst(2), .utf16LittleEndian, 2)
        }
        if n >= 3, at(0) == 0xEF, at(1) == 0xBB, at(2) == 0xBF {
            return String(data: b.dropFirst(3), encoding: .utf8)
        }
        if n >= 4 {
            if at(0) == 0 {
                // `00 00 -- --` is UTF-32-BE; `00 XX -- --` is UTF-16-BE.
                return at(1) != 0 ? wide(b, .utf16BigEndian, 2) : wide(b, .utf32BigEndian, 4)
            }
            if at(1) == 0 {
                // `XX 00 00 00` is UTF-32-LE; anything else with a null second
                // byte is UTF-16-LE.
                return (at(2) != 0 || at(3) != 0)
                    ? wide(b, .utf16LittleEndian, 2) : wide(b, .utf32LittleEndian, 4)
            }
        } else if n == 2 {
            if at(0) == 0 { return wide(b, .utf16BigEndian, 2) }
            if at(1) == 0 { return wide(b, .utf16LittleEndian, 2) }
        }
        return String(data: b, encoding: .utf8)
    }

    private func jsonHTTP(_ status: Int, _ body: JSON) -> HTTPResponse {
        HTTPResponse(
            status: status, contentType: "application/json", body: Data(body.serialize().utf8))
    }

    // MARK: - bundled resources

    /// python `templates/index.html`, byte for byte.
    static let indexTemplate: String =
        loadResource("index", "html").flatMap {
            String(data: $0, encoding: .utf8)
        } ?? ""
    /// python `favicon.png`, byte for byte.
    static let faviconBytes: Data = loadResource("favicon", "png") ?? Data()

    private static func loadResource(_ name: String, _ ext: String) -> Data? {
        Bundle.module.url(forResource: name, withExtension: ext).flatMap {
            try? Data(contentsOf: $0)
        }
    }
}

// MARK: - NIO HTTP dispatch handler

/// Collects one request (head + body + end), dispatches it to the
/// `WebServer`'s async router, and writes the response. The connection is KEPT
/// unless the peer asked otherwise (python never sends `Connection: close` on a
/// keep-alive cycle — measured 46/46; h11 appends it, and closes, exactly when
/// the request carried a `close` token or spoke HTTP/1.0). The async hop into `kernel.send` is the project's established
/// bridge idiom; the context is carried into the completion via `NIOLoopBound`.
final class HTTPDispatchHandler: ChannelInboundHandler, RemovableChannelHandler, @unchecked Sendable
{
    typealias InboundIn = HTTPServerRequestPart
    typealias OutboundOut = HTTPServerResponsePart

    private let server: WebServer
    private var head: HTTPRequestHead?
    private var bodyBuffer: ByteBuffer?

    init(server: WebServer) { self.server = server }

    func channelRead(context: ChannelHandlerContext, data: NIOAny) {
        switch unwrapInboundIn(data) {
        case .head(let h):
            head = h
            bodyBuffer = nil
        case .body(var chunk):
            if bodyBuffer == nil {
                bodyBuffer = chunk
            } else {
                bodyBuffer!.writeBuffer(&chunk)
            }
        case .end:
            guard let h = head else { return }
            let bodyData: Data? = bodyBuffer.flatMap {
                $0.getData(at: $0.readerIndex, length: $0.readableBytes)
            }
            var headers: [String: String] = [:]
            for (name, value) in h.headers { headers[name] = value }
            let req = HTTPRequest(
                method: h.method.rawValue, path: h.uri, headers: headers, body: bodyData)
            head = nil
            bodyBuffer = nil

            let server = self.server
            let bound = NIOLoopBound((context, self), eventLoop: context.eventLoop)
            let eventLoop = context.eventLoop
            // h11 `_keep_alive`, which uvicorn serves behind: a `Connection:
            // close` token closes, and an HTTP/1.0 peer ALWAYS closes (h11
            // deliberately does not support 1.0 keep-alive, where NIO's
            // `isKeepAlive` honours `Connection: keep-alive` on 1.0).
            let closeToken = h.headers["connection"].contains {
                $0.split(separator: ",").contains { t in
                    t.trimmingCharacters(in: .whitespaces).lowercased() == "close"
                }
            }
            let keepAlive =
                !closeToken && (h.version.major, h.version.minor) >= (1, 1)
            // The web boot pid parents every request task: python serves under
            // `CURRENT_TASK.scope`, so a REST-originated dispatch is a CHILD of
            // the boot task, not a root.
            let parent = server.requestParent
            Task {
                let resp = await KernelTaskLocals.$currentTask.withValue(parent) {
                    await server.handle(request: req)
                }
                eventLoop.execute {
                    let (ctx, handler) = bound.value
                    handler.writeResponse(context: ctx, response: resp, keepAlive: keepAlive)
                }
            }
        }
    }

    private func writeResponse(
        context: ChannelHandlerContext, response: HTTPResponse, keepAlive: Bool
    ) {
        // python's header order after uvicorn's own: date, server, the
        // handler's extras, content-length, content-type. Names lowercase.
        var headers = HTTPHeaders()
        headers.add(name: "date", value: httpDate())
        headers.add(name: "server", value: "uvicorn")
        for (k, v) in response.extraHeaders { headers.add(name: k, value: v) }
        // starlette omits both on a 204/304 (and on a 1xx).
        let bodyless = response.status == 204 || response.status == 304 || response.status < 200
        if !bodyless {
            if response.chunked {
                headers.add(name: "transfer-encoding", value: "chunked")
            } else {
                headers.add(name: "content-length", value: String(response.body.count))
            }
        }
        if let ct = response.contentType, !bodyless {
            headers.add(name: "content-type", value: ct)
        }
        // h11 `_clean_up_response_headers_for_sending`: when the cycle will not
        // be kept alive it APPENDS `connection: close` — last, after the framing
        // headers — and uvicorn writes it lowercase like the rest.
        if !keepAlive { headers.add(name: "connection", value: "close") }
        let respHead = HTTPResponseHead(
            version: .http1_1,
            status: HTTPResponseStatus(
                statusCode: response.status, reasonPhrase: statusText(response.status)),
            headers: headers)
        context.write(wrapOutboundOut(.head(respHead)), promise: nil)
        var buf = context.channel.allocator.buffer(capacity: response.body.count)
        buf.writeBytes(response.body)
        context.write(wrapOutboundOut(.body(.byteBuffer(buf))), promise: nil)
        context.writeAndFlush(wrapOutboundOut(.end(nil))).whenComplete { _ in
            if !keepAlive { context.close(promise: nil) }
        }
    }
}

// MARK: - HTTP request/response models

public struct HTTPRequest: Sendable {
    public let method: String
    public let path: String
    public let headers: [String: String]
    public let body: Data?
}

public struct HTTPResponse: Sendable {
    let status: Int
    /// `nil` = no body at all (python's bare `Response(status_code=…)`).
    let contentType: String?
    let body: Data
    /// Extra response headers, IN ORDER (python emits a list of pairs; a Swift
    /// Dictionary reordered them).
    let extraHeaders: [(String, String)]
    /// No `content-length`, `transfer-encoding: chunked` instead — starlette's
    /// `StreamingResponse` without a declared length. The file proxy hands the
    /// length it learned from the first chunk, and a SIZE-0 file has none, so it
    /// streams chunked exactly as python does.
    let chunked: Bool

    init(
        status: Int, contentType: String?, body: Data, extraHeaders: [(String, String)] = [],
        chunked: Bool = false
    ) {
        self.status = status
        self.contentType = contentType
        self.body = body
        self.extraHeaders = extraHeaders
        self.chunked = chunked
    }
}

// MARK: - shared helpers

/// Split a request path into its path component + parsed query dict.
/// `/a/b?x=1&y=2` → (`/a/b`, [x:1, y:2]). Mirrors `urllib.parse.parse_qsl`
/// with `keep_blank_values=True`; a repeated key keeps the LAST value, as
/// starlette's `QueryParams.get` does.
func splitQuery(_ path: String) -> (String, [String: String]) {
    guard let q = path.firstIndex(of: "?") else { return (path, [:]) }
    let pathOnly = String(path[..<q])
    let queryStr = String(path[path.index(after: q)...])
    var query: [String: String] = [:]
    for pair in queryStr.split(separator: "&") {
        let kv = pair.split(separator: "=", maxSplits: 1).map(String.init)
        let key = unquotePlus(kv[0])
        query[key] = kv.count == 2 ? unquotePlus(kv[1]) : ""
    }
    return (pathOnly, query)
}

/// `urllib.parse.unquote_plus` — `+` is a space, then percent-decoding.
private func unquotePlus(_ s: String) -> String {
    let plussed = s.replacingOccurrences(of: "+", with: " ")
    return plussed.removingPercentEncoding ?? plussed
}

/// Decode a header value the way python's ASGI layer does: byte-for-byte
/// latin-1. NIO hands us the bytes already decoded as UTF-8, so re-read each
/// UTF-8 byte as one latin-1 code point.
func latin1(_ s: String) -> String {
    String(String.UnicodeScalarView(s.utf8.map { Unicode.Scalar($0) }))
}

/// uvicorn's `date` header — `email.utils.formatdate(usegmt=True)`:
/// `Wed, 16 Sep 2026 09:41:02 GMT`, C-locale names, always GMT.
func httpDate(_ now: Date = Date()) -> String {
    let days = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"]
    let months = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ]
    var cal = Calendar(identifier: .gregorian)
    cal.timeZone = TimeZone(secondsFromGMT: 0) ?? .gmt
    let c = cal.dateComponents(
        [.year, .month, .day, .hour, .minute, .second, .weekday], from: now)
    let two = { (n: Int) in n < 10 ? "0\(n)" : "\(n)" }
    return "\(days[(c.weekday ?? 1) - 1]), \(two(c.day ?? 1)) \(months[(c.month ?? 1) - 1]) "
        + "\(c.year ?? 1970) \(two(c.hour ?? 0)):\(two(c.minute ?? 0)):\(two(c.second ?? 0)) GMT"
}

private func statusText(_ status: Int) -> String {
    switch status {
    case 200: return "OK"
    case 201: return "Created"
    case 204: return "No Content"
    case 307: return "Temporary Redirect"
    case 400: return "Bad Request"
    case 403: return "Forbidden"
    case 404: return "Not Found"
    case 405: return "Method Not Allowed"
    case 500: return "Internal Server Error"
    default: return "Status"
    }
}
