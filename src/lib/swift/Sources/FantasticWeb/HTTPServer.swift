// HTTP server backed by swift-nio (cross-platform: macOS + Linux).
//
// Replaces the former Apple-only Network.framework (`NWListener`)
// implementation wholesale — swift-nio is the project's existing
// cross-platform networking layer (the kernel_bridge already uses it),
// so the swift kernel now serves HTTP on Linux too, at full parity with
// python/rust. The route surface + wire behavior are unchanged.
//
// Routes (match python/bundled_agents/web/host/app.py — the reference):
//   GET  /                          → root link tree (agent index html)
//   GET  /_fantastic/transport.js   → transport.js
//   GET  /favicon.png               → 404 (not served)
//   GET  /_assets/<name>            → vendored Three.js / xterm assets
//   GET  /<agent_id>/               → kernel.send(agentId, render_html)
//   GET  /<agent_id>/file/<path>    → kernel.send(agentId, read_stream)
//   <method> /<surface>/<...>       → dynamic child routes (web_rest), via
//                                     the owner's `handle_route` verb
//   GET  /<agent_id>/ws             → WebSocket upgrade → the shared proxy
//                                     (web_ws), see WebSocket.swift

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

/// A route contributed by a child agent (web_ws / web_rest), pulled
/// at host boot via the child's `get_routes` verb. `kind` selects the
/// host's shared handler: `.websocket` runs the shared WS proxy;
/// `.http` calls the owner's `handle_route` verb.
public struct RouteSpec: Sendable {
    public let kind: RouteKind
    public let method: String  // uppercased; "GET" for websocket upgrades
    public let template: [String]  // path segments; `{name}` captures a param
    public let ownerId: AgentId

    public init(kind: RouteKind, method: String, path: String, ownerId: AgentId) {
        self.kind = kind
        self.method = method.uppercased()
        self.template = RouteSpec.segments(path)
        self.ownerId = ownerId
    }

    static func segments(_ path: String) -> [String] {
        path.split(separator: "/", omittingEmptySubsequences: true).map(String.init)
    }

    /// Match request `segments` against this template. Returns the
    /// captured `{param}` values, or nil if no match.
    func match(method: String, segments: [String]) -> [String: String]? {
        guard self.method == method.uppercased() else { return nil }
        guard template.count == segments.count else { return nil }
        var params: [String: String] = [:]
        for (t, s) in zip(template, segments) {
            if t.hasPrefix("{") && t.hasSuffix("}") {
                params[String(t.dropFirst().dropLast())] = s
            } else if t != s {
                return nil
            }
        }
        return params
    }
}

/// Server lifecycle owner. Started by `WebBundle.boot`, stopped by
/// `WebBundle.shutdown`. One instance per `web.tools` agent.
public final class WebServer: @unchecked Sendable {
    let kernel: Kernel
    private let agentId: AgentId
    private let lock = NSLock()
    private var channel: Channel?

    /// Process-shared event loop group — never shut down (mirrors the
    /// kernel_bridge's static-shared convention + NIO best practice).
    static let group: EventLoopGroup = MultiThreadedEventLoopGroup.singleton

    /// Dynamic routes contributed by child surface agents (web_ws /
    /// web_rest), pulled at host boot via each child's `get_routes`.
    /// The host serves only rendering + file routes natively.
    private var dynamicRoutes: [RouteSpec] = []

    public init(kernel: Kernel, agentId: AgentId) {
        self.kernel = kernel
        self.agentId = agentId
    }

    /// Replace the routes owned by `owner` with `specs` (unmount-first,
    /// so re-mounting is idempotent).
    public func mountRoutes(_ specs: [RouteSpec], for owner: AgentId) {
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

    func matchRoute(method: String, segments: [String]) -> (RouteSpec, [String: String])? {
        lock.lock()
        let routes = dynamicRoutes
        lock.unlock()
        for spec in routes {
            if let params = spec.match(method: method, segments: segments) {
                return (spec, params)
            }
        }
        return nil
    }

    /// Start the listener. If `portHint` is 0, the OS picks any free
    /// port. Returns the actual bound port. Synchronous (binds + waits)
    /// so `WebBundle.boot` gets the port immediately.
    @discardableResult
    public func start(portHint: UInt16) throws -> UInt16 {
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
        let bound = try bootstrap.bind(host: "0.0.0.0", port: Int(portHint)).wait()
        lock.lock()
        channel = bound
        lock.unlock()
        let resolvedPort = UInt16(bound.localAddress?.port ?? 0)
        kernel.setHttpPort(resolvedPort)
        if let agent = kernel.agent(agentId) {
            agent.updateMeta([
                "port": .integer(Int64(resolvedPort)),
                "running": .bool(true),
            ])
        }
        return resolvedPort
    }

    public func stop() {
        lock.lock()
        let ch = channel
        channel = nil
        lock.unlock()
        ch?.close(promise: nil)
        kernel.setHttpPort(0)
        if let agent = kernel.agent(agentId) {
            agent.updateMeta(["running": .bool(false)])
        }
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
                let segs = RouteSpec.segments(splitQuery(head.uri).0)
                if let (spec, params) = self.matchRoute(method: "GET", segments: segs),
                    spec.kind == .websocket, params["host_id"] != nil
                {
                    // Empty headers ⇒ upgrade approved; NIO adds Sec-WebSocket-Accept.
                    return channel.eventLoop.makeSucceededFuture(HTTPHeaders())
                }
                return channel.eventLoop.makeSucceededFuture(nil)
            },
            upgradePipelineHandler: { [weak self] channel, head in
                guard let self = self,
                    case let segs = RouteSpec.segments(splitQuery(head.uri).0),
                    let (spec, params) = self.matchRoute(method: "GET", segments: segs),
                    let hostId = params["host_id"]
                else {
                    return channel.eventLoop.makeSucceededVoidFuture()
                }
                return channel.pipeline.addHandler(
                    WebSocketProxyHandler(hostId: hostId, legId: spec.ownerId, kernel: kernel))
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

    func handle(request: HTTPRequest) async -> HTTPResponse {
        let (pathOnly, query) = splitQuery(request.path)
        let segments = RouteSpec.segments(pathOnly)

        // ── Built-in rendering routes (host owns these) ──
        switch (request.method, pathOnly) {
        case ("GET", "/"):
            return await serveIndex()
        case ("GET", "/_fantastic/transport.js"):
            return HTTPResponse(
                status: 200, contentType: "application/javascript",
                body: WebAssets.transportJS.data(using: .utf8) ?? Data())
        case ("GET", "/favicon.png"):
            return HTTPResponse(status: 404, contentType: "text/plain", body: Data())
        case ("GET", let path) where path.hasPrefix("/_assets/"):
            if let asset = WebAssets.body(forPath: path) {
                return HTTPResponse(
                    status: 200, contentType: asset.contentType,
                    body: asset.body.data(using: .utf8) ?? Data(),
                    extraHeaders: ["Cache-Control": "public, max-age=31536000, immutable"])
            }
            return HTTPResponse(status: 404, contentType: "text/plain", body: Data())
        default:
            break
        }

        // ── Dynamic .http surfaces (web_rest children) ──
        if let (spec, params) = matchRoute(method: request.method, segments: segments),
            spec.kind == .http
        {
            return await dispatchHTTPRoute(
                spec: spec, params: params, query: query, request: request)
        }

        // ── Built-in agent rendering routes (GET /<id>/, /<id>/file/<path>) ──
        return await serveAgentRoute(request: request)
    }

    /// Forward a matched HTTP route to its owning child agent via the
    /// `handle_route` verb → `{status, content_type, body}`.
    private func dispatchHTTPRoute(
        spec: RouteSpec, params: [String: String], query: [String: String],
        request: HTTPRequest
    ) async -> HTTPResponse {
        let bodyStr = request.body.flatMap { String(data: $0, encoding: .utf8) } ?? ""
        var paramsObj: OrderedDictionary<String, JSON> = [:]
        for (k, v) in params { paramsObj[k] = .string(v) }
        var queryObj: OrderedDictionary<String, JSON> = [:]
        for (k, v) in query { queryObj[k] = .string(v) }
        let reply = await kernel.send(
            spec.ownerId,
            .object([
                "type": .string("handle_route"),
                "method": .string(request.method),
                "path": .string(request.path),
                "params": .object(paramsObj),
                "query": .object(queryObj),
                "body": .string(bodyStr),
            ]))
        let status = Int(reply["status"].asInt ?? 200)
        let contentType = reply["content_type"].asString ?? "application/json"
        let bodyOut = reply["body"].asString ?? ""
        return HTTPResponse(
            status: status, contentType: contentType,
            body: bodyOut.data(using: .utf8) ?? Data())
    }

    private func serveIndex() async -> HTTPResponse {
        let listed = await kernel.send(
            AgentId("core"), .object(["type": .string("list_agents")]))
        let ids = (listed["agents"].asArray ?? []).compactMap { $0["id"].asString }
        let html = """
            <!doctype html><html><head><title>fantastic</title></head>
            <body>
            <h1>fantastic kernel</h1>
            <ul>
            \(ids.map { "<li><a href=\"/\($0)/\">\($0)</a></li>" }.joined(separator: "\n"))
            </ul>
            </body></html>
            """
        return HTTPResponse(
            status: 200, contentType: "text/html", body: html.data(using: .utf8) ?? Data())
    }

    private func serveAgentRoute(request: HTTPRequest) async -> HTTPResponse {
        guard let firstSeg = request.firstPathSegment else {
            return HTTPResponse(status: 404, contentType: "text/plain", body: Data())
        }
        let agentTarget = AgentId(firstSeg)
        // `GET /<id>/` or `/<id>` → render_html + transport injection.
        if request.method == "GET",
            request.path == "/\(firstSeg)/" || request.path == "/\(firstSeg)"
        {
            let reply = await kernel.send(agentTarget, .object(["type": .string("render_html")]))
            if let html = reply["html"].asString {
                let injected = WebAssets.injectTransport(into: html)
                return HTTPResponse(
                    status: 200, contentType: "text/html",
                    body: injected.data(using: .utf8) ?? Data())
            }
            return HTTPResponse(
                status: 404, contentType: "text/plain",
                body: (reply["error"].asString ?? "no render_html reply").data(using: .utf8)
                    ?? Data())
        }
        // `GET /<id>/file/<path>` → pipe the agent's `read_stream` SOURCE (raw
        // bytes, chunked; gated by the served agent's OWN leg — a sealed
        // file_bridge denies → 404). No whole-file `read` fallback.
        if request.method == "GET", request.path.hasPrefix("/\(firstSeg)/file/") {
            let filePath = String(request.path.dropFirst("/\(firstSeg)/file/".count))
            var buf = Data()
            var offset = 0
            while true {
                let (meta, body) = await kernel.sendWithBinary(
                    agentTarget,
                    .object([
                        "type": .string("read_stream"), "path": .string(filePath),
                        "offset": .integer(Int64(offset)), "length": .integer(262144),
                    ]), Data())
                if let err = meta["error"].asString {
                    return HTTPResponse(
                        status: 404, contentType: "text/plain",
                        body: err.data(using: .utf8) ?? Data())
                }
                buf.append(body)
                offset = Int(meta["next_offset"].asInt ?? Int64(offset))
                if meta["eof"].asBool ?? true { break }
            }
            return HTTPResponse(status: 200, contentType: guessMime(path: filePath), body: buf)
        }
        return HTTPResponse(
            status: 404, contentType: "text/plain", body: "no route".data(using: .utf8) ?? Data())
    }
}

// MARK: - NIO HTTP dispatch handler

/// Collects one request (head + body + end), dispatches it to the
/// `WebServer`'s async router, and writes the response — then closes
/// (one request per connection, `Connection: close`, mirroring the
/// former hand-rolled server). The async hop into `kernel.send` is the
/// project's established bridge idiom; the context is carried into the
/// completion via `NIOLoopBound` (only ever touched on its event loop).
final class HTTPDispatchHandler: ChannelInboundHandler, RemovableChannelHandler, @unchecked Sendable {
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
            Task {
                let resp = await server.handle(request: req)
                eventLoop.execute {
                    let (ctx, handler) = bound.value
                    handler.writeResponse(context: ctx, response: resp)
                }
            }
        }
    }

    private func writeResponse(context: ChannelHandlerContext, response: HTTPResponse) {
        var headers = HTTPHeaders()
        headers.add(name: "Content-Type", value: response.contentType)
        headers.add(name: "Content-Length", value: String(response.body.count))
        headers.add(name: "Connection", value: "close")
        for (k, v) in response.extraHeaders { headers.add(name: k, value: v) }
        let respHead = HTTPResponseHead(
            version: .http1_1,
            status: HTTPResponseStatus(statusCode: response.status, reasonPhrase: statusText(response.status)),
            headers: headers)
        context.write(wrapOutboundOut(.head(respHead)), promise: nil)
        var buf = context.channel.allocator.buffer(capacity: response.body.count)
        buf.writeBytes(response.body)
        context.write(wrapOutboundOut(.body(.byteBuffer(buf))), promise: nil)
        context.writeAndFlush(wrapOutboundOut(.end(nil))).whenComplete { _ in
            context.close(promise: nil)
        }
    }
}

// MARK: - HTTP request/response models

public struct HTTPRequest: Sendable {
    public let method: String
    public let path: String
    public let headers: [String: String]
    public let body: Data?

    /// First path segment after the leading `/`. Returns nil for `/`.
    public var firstPathSegment: String? {
        let trimmed = path.hasPrefix("/") ? String(path.dropFirst()) : path
        guard let idx = trimmed.firstIndex(of: "/") else {
            return trimmed.isEmpty ? nil : trimmed
        }
        return String(trimmed[..<idx])
    }
}

public struct HTTPResponse: Sendable {
    let status: Int
    let contentType: String
    let body: Data
    let extraHeaders: [String: String]

    init(status: Int, contentType: String, body: Data, extraHeaders: [String: String] = [:]) {
        self.status = status
        self.contentType = contentType
        self.body = body
        self.extraHeaders = extraHeaders
    }
}

// MARK: - shared helpers

/// Split a request path into its path component + parsed query dict.
/// `/a/b?x=1&y=2` → (`/a/b`, [x:1, y:2]).
func splitQuery(_ path: String) -> (String, [String: String]) {
    guard let q = path.firstIndex(of: "?") else { return (path, [:]) }
    let pathOnly = String(path[..<q])
    let queryStr = String(path[path.index(after: q)...])
    var query: [String: String] = [:]
    for pair in queryStr.split(separator: "&") {
        let kv = pair.split(separator: "=", maxSplits: 1).map(String.init)
        if kv.count == 2 {
            query[kv[0]] = kv[1].removingPercentEncoding ?? kv[1]
        } else if kv.count == 1 {
            query[kv[0]] = ""
        }
    }
    return (pathOnly, query)
}

private func statusText(_ status: Int) -> String {
    switch status {
    case 200: return "OK"
    case 201: return "Created"
    case 204: return "No Content"
    case 400: return "Bad Request"
    case 404: return "Not Found"
    case 500: return "Internal Server Error"
    default: return "Status"
    }
}

private func guessMime(path: String) -> String {
    let ext = (path as NSString).pathExtension.lowercased()
    switch ext {
    case "html", "htm": return "text/html"
    case "css": return "text/css"
    case "js", "mjs": return "application/javascript"
    case "json": return "application/json"
    case "png": return "image/png"
    case "jpg", "jpeg": return "image/jpeg"
    case "gif": return "image/gif"
    case "svg": return "image/svg+xml"
    case "txt", "md": return "text/plain"
    case "wasm": return "application/wasm"
    default: return "application/octet-stream"
    }
}
