// HTTP server backed by swift-nio (cross-platform: macOS + Linux).
//
// Routes (python `bundled_agents/web/host/src/web/app.py` — the reference):
//   (no `/` — the host renders nothing of its own, so an undeclared `/` is a 404)
//   GET  /_assets/favicon.png       → the bundled favicon
//   GET  /{agent_id}/file/{path}    → proxy to the agent's `read_stream` SOURCE
//   <method> /<leg>/<...>           → routes a child leg declared via `get_routes`,
//                                     served by the endpoint it handed over
//   GET  <declared ws path>         → WebSocket upgrade → that route's endpoint
//                                     (web_ws: `/{host_id}/ws` → its proxy)
//
// Matching is Starlette's over the ORDERED route list (a literal path beats a
// pattern — `orderRoutes`): first path+method match
// handles; else the first path-only match answers 405 with `allow`; else (path
// ≠ `/`) the trailing slash is toggled and a match answers 307; else 404
// `{"detail":"Not Found"}`. No transport.js, no vendored asset tree, no
// server-side `/<id>/` render route.

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

/// A route contributed by a child agent (web_ws / web_rest / any leg): the
/// DECLARATION pulled at host boot via the child's `get_routes` verb, mounted
/// with the LIVE endpoint the child's bundle handed over for that path
/// (`WebRouteEndpoints`, python `route_endpoints(agent)`). A websocket route
/// upgrades onto its endpoint's handler; an HTTP route calls its endpoint.
public struct RouteSpec: Sendable {
    public let kind: RouteKind
    public let method: String  // uppercased; "GET" for websocket upgrades
    public let path: String
    let template: [PathSegment]
    public let ownerId: AgentId
    /// The live endpoint the owner handed over for `path`.
    public let endpoint: WebEndpoint

    public init(
        kind: RouteKind, method: String, path: String, ownerId: AgentId, endpoint: WebEndpoint
    ) {
        self.kind = kind
        self.method = method.uppercased()
        self.path = path
        self.template = parseTemplate(path)
        self.ownerId = ownerId
        self.endpoint = endpoint
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
/// `GET /files/file/sp%20ace.txt?v=1` names `sp ace.txt`; matching the RAW target would answer 404 for both and
/// make a query string part of the filename.
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

// MARK: - host bind + Host/Origin guard (python `web/app.py`)

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

/// python `listen_host(rec)`: the address the listener binds — the record's `bind` when
/// it names one (the container's `0.0.0.0`), else its `host` (`webHost`). The Host/Origin
/// guard judges by `host` alone, so a wildcard `bind` leaves the guard's identity alone.
public func webListenHost(_ rec: JSON) -> String {
    let v = rec["bind"]
    return pyTruthy(v) ? (v.asString ?? pyStr(v)) : webHost(rec)
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

/// python `own_authorities(rec)`: the Host values that ARE this web host —
/// `<host>:<port>` (an IPv6 literal in brackets), plus the three loopback spellings
/// when it binds loopback. Nothing else — a container port map, a LAN name, a
/// tunnel name are other authorities and are refused; a `0.0.0.0` bind answers
/// only `0.0.0.0:<port>`.
public func webOwnAuthorities(_ rec: JSON) -> Set<String> {
    let host = webHost(rec)
    let port = pyStr(rec["port"])
    var out: Set<String> = [host.contains(":") ? "[\(host)]:\(port)" : "\(host):\(port)"]
    if ["127.0.0.1", "localhost", "::1"].contains(host) {
        out.formUnion(["127.0.0.1:\(port)", "localhost:\(port)", "[::1]:\(port)"])
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
    case favicon
    case fileProxy
    case leg(RouteSpec)
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

/// A LITERAL PATH BEATS A PATTERN. Routing is first-match-wins in table order,
/// and legs append their routes in MOUNT order — which on a hydrated host is the
/// children's folder order. A `web_ws` leg declares the catch-all
/// `/{host_id}/ws`; mounted before a leg that declares the literal `/web_ui/ws`,
/// it would swallow it. So the live table is ordered by how many PARAMETERS a
/// path has — the `{` count, fewer first; equal counts keep their mount order (a
/// STABLE sort, made explicit by the index tie-break). Python `_order_routes`.
func orderRoutes(_ routes: [RouteSpec]) -> [RouteSpec] {
    stableByParams(routes, path: { $0.path })
}

/// `items` stably sorted by the `{` count of each one's path.
func stableByParams<T>(_ items: [T], path: (T) -> String) -> [T] {
    items.enumerated()
        .map { (n: path($0.element).reduce(0) { c, ch in ch == "{" ? c + 1 : c }, i: $0.offset, x: $0.element) }
        .sorted { ($0.n, $0.i) < ($1.n, $1.i) }
        .map { $0.x }
}

/// A bind the socket layer refuses before it reaches the OS.
struct BindError: Error, CustomStringConvertible {
    let description: String
}

/// Server lifecycle owner. Started by `WebBundle.boot`, stopped by
/// `WebBundle.stop`. One instance per `web.tools` agent.
public final class WebServer: @unchecked Sendable {
    let kernel: Kernel
    private let agentId: AgentId
    /// The port `rec.port` named — python binds exactly this, never a hint, and
    /// never a 16-bit truncation of it: a port outside 0-65535 fails the bind.
    public let port: Int
    /// The bind address, read off the record when the server is BUILT (python
    /// `_spawn` puts it in uvicorn's config) — what `start` binds and what the
    /// boot announcement names.
    public let host: String
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
    /// order — python's `routes_by_child`. The host serves only the four python
    /// rendering routes natively.
    private var dynamicRoutes: [RouteSpec] = []
    /// The LIVE table requests resolve against: `dynamicRoutes` re-ordered by
    /// `orderRoutes` after every mount and unmount (python's `router.routes`).
    private var liveRoutes: [RouteSpec] = []

    public init(kernel: Kernel, agentId: AgentId, port: Int) {
        self.kernel = kernel
        self.agentId = agentId
        self.port = port
        // LOOPBACK unless the record names a bind or a host (`bind: "0.0.0.0"` to publish).
        self.host = webListenHost(
            .object([
                "bind": kernel.agent(agentId)?.metaValue(forKey: "bind") ?? .null,
                "host": kernel.agent(agentId)?.metaValue(forKey: "host") ?? .null,
            ]))
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
        liveRoutes = orderRoutes(dynamicRoutes)
    }

    /// Drop every route owned by `owner`.
    public func unmountRoutes(for owner: AgentId) {
        lock.lock()
        defer { lock.unlock() }
        dynamicRoutes.removeAll { $0.ownerId == owner }
        liveRoutes = orderRoutes(dynamicRoutes)
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
        let routes = liveRoutes
        lock.unlock()
        for spec in routes where spec.kind == .websocket {
            if let params = spec.match(segments: segments) { return (spec, params) }
        }
        return nil
    }

    /// The ORDERED route table a request walks: python's four baked routes
    /// first (FastAPI appends user routes after its own `setup()` ones), then
    /// each mounted child's declared routes — the WHOLE table under the one
    /// ordering rule (`orderRoutes`), as python sorts `app.router.routes`.
    func routeTable() -> [Route] {
        lock.lock()
        let dynamic = liveRoutes
        lock.unlock()
        var table: [(path: String, route: Route)] = [
            (
                "/_assets/favicon.png",
                Route(
                    kind: .http, methods: ["GET"], template: parseTemplate("/_assets/favicon.png"),
                    target: .favicon)
            ),
            (
                "/{agent_id}/file/{path:path}",
                Route(
                    kind: .http, methods: ["GET"],
                    template: parseTemplate("/{agent_id}/file/{path:path}"), target: .fileProxy)
            ),
        ]
        for spec in dynamic {
            table.append(
                (
                    spec.path,
                    Route(
                        kind: spec.kind, methods: [spec.method], template: spec.template,
                        target: .leg(spec))
                ))
        }
        return stableByParams(table, path: { $0.path }).map { $0.route }
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
        // python's socket `bind` refuses a port outside 0-65535 (OverflowError);
        // it is never bound modulo 2^16.
        guard let port16 = UInt16(exactly: port) else {
            throw BindError(description: "bind(): port must be 0-65535.")
        }
        // `.wait()` blocks the calling thread (WebBundle.boot's task thread,
        // never an event-loop thread) until the bind resolves or throws.
        let bound = try bootstrap.bind(host: host, port: port).wait()
        lock.lock()
        channel = bound
        lock.unlock()
        kernel.setHttpPort(port16)
    }

    public func stop() {
        lock.lock()
        let ch = channel
        channel = nil
        lock.unlock()
        // DRAINS, like python's `_stop_uvicorn`: it sets `should_exit` and then
        // `await`s the serve task, so when the `stop` verb answers the port no
        // longer accepts. A fire-and-forget `close(promise: nil)` only QUEUES
        // the close on an event loop, and the next `boot` would mount its
        // child surfaces against a socket still accepting — the
        // listener the caller was told had stopped.
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

    /// python `_OriginGuard`, the HOST half: EVERY request — any
    /// method, the websocket upgrade included — whose `Host` is not one of
    /// `webOwnAuthorities` is refused. A DNS-rebinding page sends a same-origin GET
    /// with no `Origin`; its `Host` still names the attacker's domain. `true` =
    /// refused.
    func hostRefused(_ host: String) -> Bool {
        !webOwnAuthorities(record()).contains(host)
    }

    /// python `_OriginGuard`: a websocket upgrade, or an HTTP request that is not
    /// GET/HEAD/OPTIONS, from a FOREIGN browser origin is refused. `true` = refused.
    func originRefused(websocket: Bool, method: String, origin: String?) -> Bool {
        let gated = websocket || !webSafeMethods.contains(method)
        return gated && !webOriginAllowed(record(), origin)
    }

    // MARK: - Pipeline

    /// Configure a child connection's pipeline: HTTP server + a WebSocket
    /// upgrade path (the upgrader matches a `.websocket` route in the ordered
    /// live table and, on upgrade, installs the handler that route's endpoint
    /// builds).
    private func configurePipeline(_ channel: Channel) -> EventLoopFuture<Void> {
        let kernel = self.kernel
        let upgrader = NIOWebSocketServerUpgrader(
            maxFrameSize: 1 << 24,  // 16 MiB: room for raw read_stream/write_stream chunks
            shouldUpgrade: { [weak self] channel, head in
                guard let self = self else {
                    return channel.eventLoop.makeSucceededFuture(nil)
                }
                // A foreign host or origin never upgrades: the request falls
                // through to the HTTP dispatch, which logs it and answers the 403.
                if self.hostRefused(head.headers["host"].first ?? "") {
                    return channel.eventLoop.makeSucceededFuture(nil)
                }
                let origin = head.headers["origin"].first.flatMap { $0.isEmpty ? nil : $0 }
                if self.originRefused(websocket: true, method: "GET", origin: origin) {
                    return channel.eventLoop.makeSucceededFuture(nil)
                }
                // ANY declared websocket path upgrades — literal or patterned,
                // resolved by the ordered live table (a literal beats a pattern).
                let segs = rawSegments(decodePath(head.uri))
                if self.matchWebSocket(segments: segs) != nil {
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
                    case .websocket(let endpoint) = spec.endpoint
                else {
                    return channel.eventLoop.makeSucceededVoidFuture()
                }
                let handler = endpoint(
                    WebSocketUpgrade(
                        params: params,
                        // The same boot pid an HTTP request task is parented
                        // to: python serves BOTH under `CURRENT_TASK.scope`.
                        bootParent: self.requestParent))
                // WHOLE, VALID MESSAGES ONLY: fragments reassembled
                // up to the one frame cap, then `WebSocketMessageGuard`; then
                // THE ROUTE'S OWN ENDPOINT — whatever the leg handed over for
                // this path (web_ws: its proxy, bound to the captured host_id).
                return channel.eventLoop.makeCompletedFuture {
                    try channel.pipeline.syncOperations.addHandler(
                        NIOWebSocketFrameAggregator(
                            minNonFinalFragmentSize: 0,
                            maxAccumulatedFrameCount: Int.max,
                            maxAccumulatedFrameSize: MAX_FRAME))
                    try channel.pipeline.syncOperations.addHandler(WebSocketMessageGuard())
                    try channel.pipeline.syncOperations.addHandler(handler)
                }
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

    /// NO parent: python clears `_current_task` for a REST task —
    /// the web boot dispatch has ended, so a pid parented to it was dangling.
    var requestParent: TaskId? { nil }

    /// What a request's HEAD alone decides — the origin guard, the route, a 405,
    /// a 307, a 404 — before one body byte is read: an answer, or the route to
    /// serve. `handle` serves what it resolves; `preflight` runs it on the head.
    enum Resolution {
        case respond(HTTPResponse)
        case serve(Route, params: [String: String], query: [String: String], pathOnly: String)
    }

    func resolve(method: String, path: String, headers: [String: String]) -> Resolution {
        // THE QUERY IS STRIPPED AND THE PATH PERCENT-DECODED ONCE, BEFORE
        // MATCHING — uvicorn hands the app a decoded, query-less path.
        let (_, query) = splitQuery(path)
        let pathOnly = decodePath(path)
        // THE HOST + ORIGIN GUARD, in front of every route (python `_OriginGuard`).
        var host = ""
        var origin: String?
        var websocket = false
        for (k, v) in headers {
            switch k.lowercased() {
            case "host": host = v
            case "origin": origin = v.isEmpty ? nil : v
            case "upgrade": websocket = websocket || v.lowercased() == "websocket"
            default: break
            }
        }
        let kind = websocket ? "websocket" : "http"
        // A websocket is refused on the handshake (python closes 1008 before
        // accept ⇒ uvicorn answers 403, no body); HTTP gets the coded 403.
        func refuse(_ error: String, _ hint: String) -> Resolution {
            if websocket {
                return .respond(HTTPResponse(status: 403, contentType: nil, body: Data()))
            }
            let body: JSON = .object([
                "error": .string(error),
                "reason": .string("unauthorized"),
                "hint": .string(hint),
            ])
            return .respond(
                HTTPResponse(
                    status: 403, contentType: "application/json",
                    body: Data(pyJsonDumps(body).utf8)))
        }
        if hostRefused(host) {
            let shown = pyRepr(.string(host))
            FileHandle.standardOutput.write(
                Data(
                    "  [web] \(agentId.value): refused \(kind) \(pathOnly) for foreign host \(shown)\n"
                        .utf8))
            return refuse(
                "host \(shown) is not this server's own authority",
                "address the host by its bound host:port (a loopback bind also answers "
                    + "localhost, 127.0.0.1 and [::1] on that port)")
        }
        if originRefused(websocket: websocket, method: method, origin: origin) {
            let shown = pyRepr(origin.map { JSON.string($0) } ?? .null)
            FileHandle.standardOutput.write(
                Data(
                    "  [web] \(agentId.value): refused \(kind) \(pathOnly) from foreign origin \(shown)\n"
                        .utf8))
            return refuse(
                "origin \(shown) is not allowed on this host",
                "add it to the web record's allowed_origins")
        }
        let segments = rawSegments(pathOnly)
        let table = routeTable()

        // Starlette `Router.app`: first FULL match wins; remember the first
        // path-only match for the 405.
        var partial: Route?
        for route in table where route.kind == .http {
            guard let params = matchTemplate(route.template, segments) else { continue }
            if route.methods == nil || route.methods!.contains(method) {
                return .serve(route, params: params, query: query, pathOnly: pathOnly)
            }
            if partial == nil { partial = route }
        }
        if let partial = partial {
            let allow = (partial.methods ?? []).sorted().joined(separator: ", ")
            return .respond(
                HTTPResponse(
                    status: 405, contentType: "application/json",
                    body: Data(
                        JSON.object(["detail": .string("Method Not Allowed")]).serialize().utf8),
                    extraHeaders: [("allow", allow)]))
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
                for (k, v) in headers where k.lowercased() == "host" { host = v }
                let q = path.firstIndex(of: "?").map { String(path[$0...]) } ?? ""
                return .respond(
                    HTTPResponse(
                        status: 307, contentType: nil, body: Data(),
                        extraHeaders: [("location", "http://\(host)\(toggled)\(q)")]))
            }
        }
        // FastAPI's 404 handler — `{"detail":"Not Found"}`, 22 bytes.
        return .respond(
            HTTPResponse(
                status: 404, contentType: "application/json",
                body: Data(JSON.object(["detail": .string("Not Found")]).serialize().utf8)))
    }

    func handle(request: HTTPRequest) async -> HTTPResponse {
        switch resolve(method: request.method, path: request.path, headers: request.headers) {
        case .respond(let r): return r
        case .serve(let route, let params, let query, let pathOnly):
            return await serve(
                route: route, params: params, query: query, pathOnly: pathOnly, request: request)
        }
    }

    /// What the host does with a request's BODY, decided on its head.
    enum Preflight {
        /// Answer now; the body is never buffered (its bytes are drained unread).
        case respond(HTTPResponse)
        /// Read the body — at most `limit` bytes (past it: 413).
        case read(limit: Int)
        /// Serve the route without a body: its bytes are drained unread.
        case drain
    }

    /// THE GATE FIRST, THEN THE BODY (python
    /// `web_rest._rest_call`). A web_rest POST is judged by the leg's ingress
    /// rule on what the request says up front — its target and its
    /// `X-Fantastic-Auth` credential (the rule sees `kind: call`, verb `""`) —
    /// and only an admitted request's body is read, at most `restBodyLimit`
    /// bytes (io_bridge's `MAX_FRAME`); a larger one is refused 413 (a declared
    /// `Content-Length` over it unread). Buffering and parsing the whole body
    /// before the gate would let a client the leg was about to refuse make the
    /// kernel hold any amount of data. Everything the head alone answers (origin guard,
    /// 404/405/307) is answered here too, with nothing read.
    ///
    /// NO OTHER ROUTE READS A BODY: the favicon, the file
    /// proxy and web_rest's GET `_reflect` routes are served on the head, and
    /// whatever body a client sends them is drained unread — python and rust never
    /// read it either. Buffering it unbounded would let any client that reaches
    /// the port grow the kernel until it is killed.
    func preflight(_ head: HTTPRequestHead) -> Preflight {
        var headers: [String: String] = [:]
        for (name, value) in head.headers { headers[name] = value }
        let method = head.method.rawValue
        switch resolve(method: method, path: head.uri, headers: headers) {
        case .respond(let r):
            return .respond(r)
        case .serve(let route, let params, _, _):
            guard method == "POST", case .leg(let spec) = route.target,
                case .http = spec.endpoint
            else { return .drain }
            var lower: [String: String] = [:]
            for (k, v) in headers { lower[k.lowercased()] = latin1(v) }
            if let denied = gateWebLeg(
                kernel: kernel, legId: spec.ownerId, kind: "call",
                target: params["target_id"] ?? "", verb: "", token: lower["x-fantastic-auth"])
            {
                return .respond(jsonHTTP(403, denied))
            }
            if let declared = lower["content-length"], !declared.isEmpty,
                declared.allSatisfy({ $0.isASCII && $0.isNumber }),
                (Int(declared) ?? Int.max) > Self.restBodyLimit
            {
                return .respond(Self.restTooLarge)
            }
            return .read(limit: Self.restBodyLimit)
        }
    }

    /// The 413 a web_rest body over `restBodyLimit` gets. Python `_too_large`.
    static let restTooLarge = HTTPResponse(
        status: 413, contentType: "application/json",
        body: Data(
            JSON.object([
                "error": .string(
                    "web_rest: the request body exceeds \(restBodyLimit) bytes — the one frame "
                        + "cap every wire leg holds (io_bridge MAX_FRAME); send larger data in "
                        + "chunks (write_stream)"),
                "reason": .string("transport_error"),
                "limit": .integer(Int64(restBodyLimit)),
            ]).serialize().utf8))


    private func serve(
        route: Route, params: [String: String], query: [String: String], pathOnly: String,
        request: HTTPRequest
    ) async -> HTTPResponse {
        switch route.target {
        case .favicon:
            return HTTPResponse(status: 200, contentType: "image/png", body: Self.faviconBytes)
        case .fileProxy:
            return await serveFileProxy(
                agentId: params["agent_id"] ?? "", path: params["path"] ?? "")
        case .leg(let spec):
            return await dispatchLegRoute(
                spec: spec, params: params, query: query, pathOnly: pathOnly, request: request)
        }
    }

    /// python `agent_file` — pipe the agent's `read_stream` SOURCE.
    ///
    /// READ BY THE WEB HOST: the request is handed to it as
    /// `fetch_file` (this route is the transport — the operator), and its handler
    /// sends `read_stream` AS ITSELF, so the target's `callable_by` decides whether
    /// `web` may read it at all — never the operator, which would hand the browser
    /// the origin root of trust.
    private func serveFileProxy(agentId: String, path: String) async -> HTTPResponse {
        guard kernel.agent(AgentId(agentId)) != nil else { return Self.emptyNotFound }
        let web = self.agentId
        @Sendable func readChunk(_ offset: Int) async -> (JSON, Data?) {
            let out = await kernel.send([
                Item(
                    web,
                    .object([
                        "type": .string("fetch_file"), "agent_id": .string(agentId),
                        "path": .string(path), "offset": .integer(Int64(offset)),
                        "length": .integer(262144),
                    ]), rawReply: true)
            ])[0]
            // python `isinstance(first["bytes"], bytes)`: the chunk at `bytes`
            return (out.result, bodyAt(out.body, "bytes"))
        }
        let (meta, body) = await readChunk(0)
        // THE SOURCE TEST IS THE CARRIER, python's `isinstance(first["bytes"],
        // bytes)`: anything that did not answer with a CHUNK is a 404 — a denied
        // gate, a path escape, a missing file, a non-server. An EMPTY chunk is
        // still a chunk.
        //
        // A GATE denial carries its reason in the body — this kernel teaches
        // through denials. A MISSING FILE stays a bare 404: no body, no existence
        // leak, no path probing.
        guard let first = body else {
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
        // STREAMED, chunk by chunk, as python's `StreamingResponse` pulls its
        // generator: the next `read_stream` runs only when the writer asks for
        // it, nothing is held whole in RAM, and a chunk that fails mid-file ends
        // the body SHORT of the declared length — never a shorter file dressed
        // up as complete.
        let cursor = ChunkCursor(meta)
        let rest = AsyncStream<Data>(unfolding: {
            guard let next = cursor.nextOffset() else { return nil }
            let (m, b) = await readChunk(next)
            guard let chunk = b else { return nil }
            cursor.advance(m)
            return chunk
        })
        // python types the proxy with `mimetypes.guess_type(path)` and falls
        // back to octet-stream; starlette appends the charset to a `text/*`.
        var mime = Mimetypes.guessType(path) ?? "application/octet-stream"
        if mime.hasPrefix("text/") && !mime.contains("charset=") { mime += "; charset=utf-8" }
        return HTTPResponse(
            status: 200, contentType: mime, body: first, chunked: size == 0,
            stream: rest, declaredLength: size == 0 ? nil : Int(size))
    }

    /// python `Response(status_code=404)` — no body, no content-type.
    static let emptyNotFound = HTTPResponse(status: 404, contentType: nil, body: Data())

    // MARK: - leg routes

    /// The most body bytes a web_rest request may carry: io_bridge's one frame
    /// cap. Python `web_rest.MAX_BODY`.
    static let restBodyLimit = MAX_FRAME

    static let octetType = "application/octet-stream"
    static let payloadHeader = "x-fantastic-payload"
    static let bytesFieldHeader = "x-fantastic-bytes-field"

    /// Serve a matched leg route. The JSON leg is the endpoint the leg handed
    /// over for this path (python's mounted callable); the octet leg is the
    /// host's, because a JSON `{status, content_type, body}` reply cannot carry
    /// raw bytes or response headers.
    private func dispatchLegRoute(
        spec: RouteSpec, params: [String: String], query: [String: String], pathOnly: String,
        request: HTTPRequest
    ) async -> HTTPResponse {
        let owner = spec.ownerId
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

        // THE BODY DECODES LIKE `json.loads(bytes)`: CPython detects the
        // encoding from the BOM / the null-byte pattern of the first four bytes,
        // so a UTF-8-BOM, UTF-16 or UTF-32 body is ordinary JSON to python.
        // Bytes that will not decode are the ENDPOINT's to answer — python's
        // `web_rest` catches the `UnicodeDecodeError` with the `JSONDecodeError`
        // and answers its own 400 — so the route gets `body: null`, meaning "not
        // text", never a guessed string.
        let bodyText = Self.decodeJSONBody(request.body ?? Data())

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
            "body": bodyText.map { JSON.string($0) } ?? .null,
        ])
        guard case .http(let endpoint) = spec.endpoint else {
            return jsonHTTP(
                500, .object(["error": .string("no http endpoint for \(spec.path)")]))
        }
        let (reply, octet) = await endpoint(routeRequest)
        let status = Int(reply["status"].asInt ?? 200)
        // ONE DECISION, shared with the octet leg: a reply that HOLDS a chunk is
        // octet, whatever its length.
        if let octet { return Self.octetReply(octet) }
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
    static func octetReply(_ octet: OctetReply) -> HTTPResponse {
        HTTPResponse(
            status: 200, contentType: octetType, body: octet.bytes,
            extraHeaders: [(payloadHeader, octet.payloadHeader), (bytesFieldHeader, octet.field)])
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
        guard let parsed = try? JSON.parseStrict(raw.isEmpty ? "{}" : raw) else {
            return jsonHTTP(
                400, .object(["error": .string("web_rest: X-Fantastic-Payload is not valid JSON")])
            )
        }
        guard case .object(var payload) = parsed else {
            return jsonHTTP(
                400,
                .object(["error": .string("web_rest: X-Fantastic-Payload must be a JSON object")]))
        }
        // python binds the body under the NAMED field (`payload[field] = body`),
        // and each handler reads its OWN key (`paste_image` reads `data`,
        // `write_stream` reads `bytes`). The field is ONE LITERAL TOP-LEVEL KEY,
        // never a dotted path: `a.b` is the key `"a.b"`, and a nested `a.b` the
        // payload header already holds is left as sent. The kernel's binary
        // channel carries the body WITH that key as its one step (`Blob(field, …)`),
        // so the target finds it exactly where python's dict holds it. The slot
        // keeps its place (or is appended) — overwriting any header value there,
        // as python's assignment does — so the call row lists the field where
        // python's dict holds it. An absent or empty header means `bytes`.
        let named = headers[Self.bytesFieldHeader] ?? ""
        let field = named.isEmpty ? "bytes" : named
        payload[field] = .null

        // GATED BEFORE THE BODY WAS READ: `preflight` ran the leg's
        // ingress rule on the head — target + X-Fantastic-Auth — exactly as for
        // the JSON leg; only an admitted request reaches here.

        // THROUGH THE BATCH: ONE binary item, so the octet leg
        // NAMES its execution like the JSON leg — a repeated
        // `X-Fantastic-Execution-Id` on the same target REPLAYS the recorded
        // outcome instead of re-running the write, and the kernel produces
        // python's own refusals for a missing `payload.type` or a rebinding id.
        // HANDED TO THE LEG (`inbound_call`, the transport's operator hand-in),
        // which sends it AS ITSELF: the body rides the binary channel at its slot
        // of the item's payload.
        var item: OrderedDictionary<String, JSON> = [
            "target": .string(target), "payload": .object(payload),
        ]
        if let execId = headers["x-fantastic-execution-id"], !execId.isEmpty {
            item["id"] = .string(execId)
        }
        if let s = headers["x-fantastic-secret"], !s.isEmpty {
            item["secret"] = .array(
                s.split(separator: ",").map { .string($0.trimmingCharacters(in: .whitespaces)) })
        }
        let out = await Inbound.handIn(
            kernel, legId: owner,
            .object(["type": .string("inbound_call"), "item": .object(item)]),
            binary: Blob(
                parts: [.key("item"), .key("payload"), .key(field)], request.body ?? Data()))
        // python answers 204 for a null reply BEFORE it looks for bytes.
        if out.result.isNull { return HTTPResponse(status: 204, contentType: nil, body: Data()) }
        // THE SAME ONE DECISION as the JSON leg (`RestOctet.split`): octet iff
        // the verb returned a CHUNK at a top-level key, the reply WITHOUT that
        // key as the header. `!outBody.isEmpty` served an empty file as JSON.
        if let chunk = out.body {
            guard let octet = RestOctet.split(out.result, chunk) else {
                return jsonHTTP(500, RestOctet.noJSONForm)
            }
            return Self.octetReply(octet)
        }
        return jsonHTTP(200, out.result)
    }

    /// CPython `json.detect_encoding` + `bytes.decode(enc, "surrogatepass")`.
    /// `nil` = the bytes do not decode, which python raises on.
    ///
    /// `surrogatepass` admits a lone surrogate here, and python's
    /// `strict_loads` then refuses it, as it refuses `NaN`/`Infinity` —
    /// so those bodies answer 400 on both. The ENCODING half below is exact.
    static func decodeJSONBody(_ b: Data) -> String? {
        if b.isEmpty { return "" }
        let n = b.count
        func at(_ i: Int) -> UInt8 { b[b.startIndex + i] }
        // BOMs first, widest first — `ff fe 00 00` is UTF-32-LE, and `ff fe`
        // alone is UTF-16-LE.
        // Foundation's decoders are LENIENT — they drop a trailing odd byte
        // instead of failing, which is the difference between python's 500 and a
        // silent 204 (`ff fe 7b`). The unit width is checked here.
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
/// keep-alive cycle; h11 appends it, and closes, exactly when
/// the request carried a `close` token or spoke HTTP/1.0). The async hop into `kernel.send` is the project's established
/// bridge idiom; the context is carried into the completion via `NIOLoopBound`.
final class HTTPDispatchHandler: ChannelInboundHandler, RemovableChannelHandler, @unchecked Sendable
{
    typealias InboundIn = HTTPServerRequestPart
    typealias OutboundOut = HTTPServerResponsePart

    private let server: WebServer
    private var head: HTTPRequestHead?
    private var bodyBuffer: ByteBuffer?
    /// The most body bytes this request may buffer (`WebServer.preflight`);
    /// `nil` = its route reads no body, so every byte of it is drained unread.
    private var bodyLimit: Int?
    /// The request was ANSWERED on its head (or past its bound): the rest of its
    /// body is drained unread, never buffered.
    private var answered = false

    init(server: WebServer) { self.server = server }

    /// h11 `_keep_alive`, which uvicorn serves behind: a `Connection: close`
    /// token closes, and an HTTP/1.0 peer ALWAYS closes (h11 deliberately does
    /// not support 1.0 keep-alive, where NIO's `isKeepAlive` honours
    /// `Connection: keep-alive` on 1.0).
    private static func keepAlive(_ h: HTTPRequestHead) -> Bool {
        let closeToken = h.headers["connection"].contains {
            $0.split(separator: ",").contains { t in
                t.trimmingCharacters(in: .whitespaces).lowercased() == "close"
            }
        }
        return !closeToken && (h.version.major, h.version.minor) >= (1, 1)
    }

    /// Answer the request NOW — before its body has arrived. The
    /// pipeline keeps reading the body to its end, which `channelRead` drains.
    private func answerEarly(
        context: ChannelHandlerContext, head h: HTTPRequestHead, _ response: HTTPResponse
    ) {
        answered = true
        bodyBuffer = nil
        writeResponse(context: context, response: response, keepAlive: Self.keepAlive(h))
    }

    func channelRead(context: ChannelHandlerContext, data: NIOAny) {
        switch unwrapInboundIn(data) {
        case .head(let h):
            head = h
            bodyBuffer = nil
            bodyLimit = nil
            answered = false
            switch server.preflight(h) {
            case .respond(let response): answerEarly(context: context, head: h, response)
            case .read(let limit): bodyLimit = limit
            case .drain: break
            }
        case .body(var chunk):
            if answered { return }  // drained unread
            guard let limit = bodyLimit else { return }  // no body read: drained unread
            if bodyBuffer == nil {
                bodyBuffer = chunk
            } else {
                bodyBuffer!.writeBuffer(&chunk)
            }
            if let h = head, bodyBuffer!.readableBytes > limit {
                answerEarly(context: context, head: h, WebServer.restTooLarge)
            }
        case .end:
            if answered {
                head = nil
                answered = false
                return
            }
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
            let keepAlive = Self.keepAlive(h)
            // The web boot pid parents every request task: python serves under
            // `CURRENT_TASK.scope`, so a REST-originated dispatch is a CHILD of
            // the boot task, not a root.
            let parent = server.requestParent
            Task {
                await KernelTaskLocals.$currentTask.withValue(parent) {
                    let resp = await server.handle(request: req)
                    eventLoop.execute {
                        let (ctx, handler) = bound.value
                        handler.writeResponse(context: ctx, response: resp, keepAlive: keepAlive)
                    }
                    // A streamed body's later chunks are read here, under the
                    // same request scope, as the writer pulls them.
                    if let stream = resp.stream {
                        await HTTPDispatchHandler.writeStreamed(
                            stream, firstCount: resp.body.count, declared: resp.declaredLength,
                            keepAlive: keepAlive, bound: bound, eventLoop: eventLoop)
                    }
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
                headers.add(
                    name: "content-length",
                    value: String(response.declaredLength ?? response.body.count))
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
        if response.stream != nil {
            // The rest follows chunk by chunk (`writeStreamed`); `end` waits.
            context.writeAndFlush(wrapOutboundOut(.body(.byteBuffer(buf))), promise: nil)
            return
        }
        context.write(wrapOutboundOut(.body(.byteBuffer(buf))), promise: nil)
        context.writeAndFlush(wrapOutboundOut(.end(nil))).whenComplete { _ in
            if !keepAlive { context.close(promise: nil) }
        }
    }

    /// Pull a streamed body to its end, one chunk written (and flushed) before
    /// the next is read. Ends the message — or, when the stream stopped short
    /// of the length the head declared, CLOSES the connection: the client sees
    /// the truncated transfer python's h11 produces, not a complete shorter one.
    static func writeStreamed(
        _ stream: AsyncStream<Data>, firstCount: Int, declared: Int?, keepAlive: Bool,
        bound: NIOLoopBound<(ChannelHandlerContext, HTTPDispatchHandler)>, eventLoop: EventLoop
    ) async {
        var written = firstCount
        for await chunk in stream {
            written += chunk.count
            let flushed: EventLoopFuture<Void> = eventLoop.flatSubmit {
                let (ctx, handler) = bound.value
                var buf = ctx.channel.allocator.buffer(capacity: chunk.count)
                buf.writeBytes(chunk)
                return ctx.writeAndFlush(handler.wrapOutboundOut(.body(.byteBuffer(buf))))
            }
            if (try? await flushed.get()) == nil { return }  // the peer is gone
        }
        let short = declared.map { written < $0 } ?? false
        eventLoop.execute {
            let (ctx, handler) = bound.value
            if short {
                ctx.close(promise: nil)
                return
            }
            ctx.writeAndFlush(handler.wrapOutboundOut(.end(nil))).whenComplete { _ in
                if !keepAlive { bound.value.0.close(promise: nil) }
            }
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
    /// The REST of a streamed body, pulled one chunk at a time after `body`
    /// (the first chunk) is written — starlette's `StreamingResponse`. `nil`
    /// for an ordinary response.
    let stream: AsyncStream<Data>?
    /// The `content-length` a streamed body DECLARES up front (python: the
    /// first chunk's `size`) — not the bytes it ends up carrying. A stream that
    /// stops short of it is a truncated transfer, and the connection closes.
    let declaredLength: Int?

    init(
        status: Int, contentType: String?, body: Data, extraHeaders: [(String, String)] = [],
        chunked: Bool = false, stream: AsyncStream<Data>? = nil, declaredLength: Int? = nil
    ) {
        self.status = status
        self.contentType = contentType
        self.body = body
        self.extraHeaders = extraHeaders
        self.chunked = chunked
        self.stream = stream
        self.declaredLength = declaredLength
    }
}

/// Where a streamed `read_stream` file stands: the offset to read next, or
/// `nil` once a chunk said `eof`. Touched only by the one pulling writer.
final class ChunkCursor: @unchecked Sendable {
    private var next: Int?
    init(_ first: JSON) { advance(first) }
    func advance(_ chunk: JSON) {
        next = pyTruthy(chunk["eof"]) ? nil : Int(chunk["next_offset"].asInt ?? 0)
    }
    func nextOffset() -> Int? { next }
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
