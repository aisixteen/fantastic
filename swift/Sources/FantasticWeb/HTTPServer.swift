// Minimal HTTP server backed by Network.framework `NWListener`.
//
// Mirrors Rust's `fantastic-web` axum routes at the wire level. No
// external dependencies — `Network` is built into every Apple
// platform. Trade-off: we hand-roll a small HTTP/1.1 parser. Worth
// it for the brain kernel's small route surface.
//
// Routes (match python/bundled_agents/web/host/app.py — the
// reference template):
//   GET  /                          → root link tree (JSON of agents)
//   GET  /_fantastic/transport.js   → transport.js
//   GET  /favicon.png               → bundled favicon
//   GET  /_assets/<name>            → vendored Three.js / xterm assets
//                                     (Swift-specific extension for
//                                     hermetic operation — Python's
//                                     canvas.html still loads these
//                                     from a CDN; intentional drift)
//   GET  /<agent_id>/               → kernel.send(agentId, render_html)
//   GET  /<agent_id>/file/...       → kernel.send(agentId, read path)
//   POST /<agent_id>/<verb>         → kernel.send(agentId, {type:verb, ...body})
//
// WebSocket upgrade (`/<agent_id>/ws`) lands in 8C — this file
// dispatches unknown upgrades through to the WS handler hook.

import FantasticJSON
import FantasticKernel
import Foundation
import Network

#if canImport(Darwin)
    import Darwin
#endif

/// Server lifecycle owner. Started by `WebBundle.boot`, stopped by
/// `WebBundle.shutdown`. One instance per `web.tools` agent.
public final class WebServer: @unchecked Sendable {
    private let kernel: Kernel
    private let agentId: AgentId
    private var listener: NWListener?
    private let lock = NSLock()
    private var connections: Set<ObjectIdentifier> = []

    /// Optional WebSocket upgrade handler installed in 8C. When
    /// non-nil, `Upgrade: websocket` requests are forwarded here.
    public var webSocketUpgrade:
        ((_ agentId: String, _ connection: NWConnection, _ request: HTTPRequest) -> Void)?

    public init(kernel: Kernel, agentId: AgentId) {
        self.kernel = kernel
        self.agentId = agentId
    }

    /// Start the listener. If `portHint` is 0, the OS picks any
    /// free port. Returns the actual bound port.
    @discardableResult
    public func start(portHint: UInt16) throws -> UInt16 {
        let parameters = NWParameters.tcp
        parameters.allowLocalEndpointReuse = true
        let port: NWEndpoint.Port = portHint == 0 ? .any : NWEndpoint.Port(rawValue: portHint)!
        let listener = try NWListener(using: parameters, on: port)
        self.listener = listener

        listener.newConnectionHandler = { [weak self] conn in
            self?.accept(conn)
        }

        // Use an NSLock-protected box for cross-thread state shared
        // with the listener callback (Swift 6 strict concurrency).
        final class StartupResult: @unchecked Sendable {
            let lock = NSLock()
            var port: UInt16 = 0
            var error: Error?
        }
        let result = StartupResult()
        let semaphore = DispatchSemaphore(value: 0)
        listener.stateUpdateHandler = { [weak listener] state in
            switch state {
            case .ready:
                if let p = listener?.port {
                    result.lock.lock()
                    result.port = p.rawValue
                    result.lock.unlock()
                }
                semaphore.signal()
            case .failed(let err):
                result.lock.lock()
                result.error = err
                result.lock.unlock()
                semaphore.signal()
            case .cancelled:
                break
            default:
                break
            }
        }
        listener.start(queue: .global())
        _ = semaphore.wait(timeout: .now() + .seconds(5))
        result.lock.lock()
        let err = result.error
        let resolvedPort = result.port
        result.lock.unlock()
        if let err = err {
            throw err
        }
        // Sync port back into the kernel + the web agent's meta.
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
        listener?.cancel()
        listener = nil
        kernel.setHttpPort(0)
        if let agent = kernel.agent(agentId) {
            agent.updateMeta(["running": .bool(false)])
        }
    }

    // MARK: - Per-connection lifecycle

    private func accept(_ connection: NWConnection) {
        let id = ObjectIdentifier(connection)
        lock.lock()
        connections.insert(id)
        lock.unlock()

        connection.stateUpdateHandler = { [weak self] state in
            switch state {
            case .ready:
                self?.readRequest(connection)
            case .cancelled, .failed:
                self?.lock.lock()
                self?.connections.remove(id)
                self?.lock.unlock()
            default:
                break
            }
        }
        connection.start(queue: .global())
    }

    private func readRequest(_ connection: NWConnection, accumulated: Data = Data()) {
        connection.receive(minimumIncompleteLength: 1, maximumLength: 16384) {
            [weak self] data, _, isComplete, error in
            guard let self = self else {
                connection.cancel()
                return
            }
            var buffer = accumulated
            if let data = data, !data.isEmpty {
                buffer.append(data)
            }
            // Try to parse — if headers aren't fully received yet,
            // keep reading.
            if let request = HTTPRequest.tryParse(buffer) {
                Task { [weak self] in
                    guard let self = self else { return }
                    await self.handle(request: request, on: connection)
                }
                return
            }
            if error != nil || isComplete {
                connection.cancel()
                return
            }
            // Need more bytes.
            self.readRequest(connection, accumulated: buffer)
        }
    }

    // MARK: - Request handling

    private func handle(request: HTTPRequest, on connection: NWConnection) async {
        // WebSocket upgrade hook (filled in by 8C).
        if request.headers["Upgrade"]?.lowercased() == "websocket",
            let agentSegment = request.firstPathSegment,
            request.path.hasSuffix("/ws"),
            let handler = webSocketUpgrade
        {
            handler(agentSegment, connection, request)
            return
        }

        let response: HTTPResponse
        switch (request.method, request.path) {
        case ("GET", "/"):
            response = await serveIndex()
        case ("GET", "/_fantastic/transport.js"):
            // URL matches python/web/host/app.py:122 — keeps the
            // bundled transport.js reachable at the same path on
            // both runtimes so identical HTML can target either.
            response = HTTPResponse(
                status: 200, contentType: "application/javascript",
                body: WebAssets.transportJS.data(using: .utf8) ?? Data())
        case ("GET", "/favicon.png"):
            response = HTTPResponse(status: 404, contentType: "text/plain", body: Data())
        case ("GET", let path) where path.hasPrefix("/_assets/"):
            if let asset = WebAssets.body(forPath: path) {
                response = HTTPResponse(
                    status: 200, contentType: asset.contentType,
                    body: asset.body.data(using: .utf8) ?? Data(),
                    extraHeaders: [
                        "Cache-Control": "public, max-age=31536000, immutable"
                    ])
            } else {
                response = HTTPResponse(status: 404, contentType: "text/plain", body: Data())
            }
        default:
            response = await serveAgentRoute(request: request)
        }
        await write(response: response, to: connection)
    }

    private func serveIndex() async -> HTTPResponse {
        let listed = await kernel.send(
            AgentId("core"), .object(["type": .string("list_agents")]))
        let ids = (listed["agents"].asArray ?? [])
            .compactMap { $0["id"].asString }
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
            status: 200, contentType: "text/html",
            body: html.data(using: .utf8) ?? Data())
    }

    private func serveAgentRoute(request: HTTPRequest) async -> HTTPResponse {
        guard let firstSeg = request.firstPathSegment else {
            return HTTPResponse(status: 404, contentType: "text/plain", body: Data())
        }
        let agentTarget = AgentId(firstSeg)
        // `GET /<id>/` → render_html
        if request.method == "GET",
            request.path == "/\(firstSeg)/" || request.path == "/\(firstSeg)"
        {
            let reply = await kernel.send(
                agentTarget, .object(["type": .string("render_html")]))
            if let html = reply["html"].asString {
                let injected = WebAssets.injectTransport(into: html)
                return HTTPResponse(
                    status: 200, contentType: "text/html",
                    body: injected.data(using: .utf8) ?? Data())
            }
            return HTTPResponse(
                status: 404, contentType: "text/plain",
                body: (reply["error"].asString ?? "no render_html reply")
                    .data(using: .utf8) ?? Data())
        }
        // `GET /<id>/file/<path>` → read verb
        if request.method == "GET",
            request.path.hasPrefix("/\(firstSeg)/file/")
        {
            let filePath = String(
                request.path.dropFirst("/\(firstSeg)/file/".count))
            let reply = await kernel.send(
                agentTarget,
                .object([
                    "type": .string("read"),
                    "path": .string(filePath),
                ]))
            if let content = reply["content"].asString {
                return HTTPResponse(
                    status: 200,
                    contentType: guessMime(path: filePath),
                    body: content.data(using: .utf8) ?? Data())
            }
            if let b64 = reply["image_base64"].asString,
                let bytes = Data(base64Encoded: b64)
            {
                return HTTPResponse(
                    status: 200,
                    contentType: reply["mime"].asString ?? "application/octet-stream",
                    body: bytes)
            }
            return HTTPResponse(
                status: 404, contentType: "text/plain",
                body: (reply["error"].asString ?? "no content").data(using: .utf8)
                    ?? Data())
        }
        // `POST /<id>/<verb>` → kernel.send(agent, {type:verb, ...body})
        if request.method == "POST" {
            let pathBits = request.path.split(separator: "/", omittingEmptySubsequences: true)
                .map(String.init)
            guard pathBits.count >= 2 else {
                return HTTPResponse(
                    status: 400, contentType: "text/plain",
                    body: "expected /<id>/<verb>".data(using: .utf8) ?? Data())
            }
            let verb = pathBits[1]
            var payload: JSON = .object(["type": .string(verb)])
            if let body = request.body, !body.isEmpty,
                let parsed = try? JSON.parse(body),
                case let .object(dict) = parsed
            {
                var merged = dict
                merged["type"] = .string(verb)
                payload = .object(merged)
            }
            let reply = await kernel.send(agentTarget, payload)
            return HTTPResponse(
                status: 200, contentType: "application/json",
                body: reply.serialize().data(using: .utf8) ?? Data())
        }
        return HTTPResponse(
            status: 404, contentType: "text/plain",
            body: "no route".data(using: .utf8) ?? Data())
    }

    private func write(response: HTTPResponse, to connection: NWConnection) async {
        let headers =
            response.extraHeaders
            .map { "\($0.key): \($0.value)\r\n" }
            .joined()
        let head = """
            HTTP/1.1 \(response.status) \(statusText(response.status))\r
            Content-Type: \(response.contentType)\r
            Content-Length: \(response.body.count)\r
            Connection: close\r
            \(headers)\r

            """
        var out = head.data(using: .utf8) ?? Data()
        out.append(response.body)
        connection.send(
            content: out,
            completion: .contentProcessed { _ in
                connection.cancel()
            })
    }
}

// MARK: - HTTP types

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

    /// Attempt to parse `data` as a complete HTTP/1.1 request.
    /// Returns nil if the buffer doesn't yet contain a full message.
    static func tryParse(_ data: Data) -> HTTPRequest? {
        guard let crlfCrlf = rangeOfCRLFCRLF(in: data) else { return nil }
        let headerData = data.prefix(upTo: crlfCrlf.lowerBound)
        guard let headerStr = String(data: headerData, encoding: .utf8) else { return nil }
        let lines = headerStr.components(separatedBy: "\r\n")
        guard let requestLine = lines.first else { return nil }
        let parts = requestLine.split(separator: " ", maxSplits: 2)
            .map(String.init)
        guard parts.count >= 2 else { return nil }
        let method = parts[0]
        let path = parts[1]
        var headers: [String: String] = [:]
        for line in lines.dropFirst() where !line.isEmpty {
            guard let colon = line.firstIndex(of: ":") else { continue }
            let key = String(line[..<colon]).trimmingCharacters(in: .whitespaces)
            let value = String(line[line.index(after: colon)...])
                .trimmingCharacters(in: .whitespaces)
            headers[key] = value
        }
        // Body extraction if Content-Length present.
        var body: Data? = nil
        if let lenStr = headers["Content-Length"], let len = Int(lenStr), len > 0 {
            let bodyStart = crlfCrlf.upperBound
            let available = data.count - bodyStart
            if available < len {
                return nil  // need more bytes
            }
            body = data[bodyStart..<(bodyStart + len)]
        }
        return HTTPRequest(method: method, path: path, headers: headers, body: body)
    }
}

public struct HTTPResponse: Sendable {
    let status: Int
    let contentType: String
    let body: Data
    let extraHeaders: [String: String]

    init(
        status: Int,
        contentType: String,
        body: Data,
        extraHeaders: [String: String] = [:]
    ) {
        self.status = status
        self.contentType = contentType
        self.body = body
        self.extraHeaders = extraHeaders
    }
}

private func rangeOfCRLFCRLF(in data: Data) -> Range<Data.Index>? {
    let needle: [UInt8] = [0x0D, 0x0A, 0x0D, 0x0A]
    guard data.count >= needle.count else { return nil }
    for i in data.startIndex...(data.endIndex - needle.count) {
        if data[i..<(i + needle.count)].elementsEqual(needle) {
            return i..<(i + needle.count)
        }
    }
    return nil
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
