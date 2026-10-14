// The MCP wire — Streamable HTTP, DUAL-ERA. Python
// `mcp_client/wire.py`, text for text: the modern protocol 2026-07-28 first, the
// `initialize`-era ones (`legacy`) when the server speaks only those — the spec's
// "dual-era client" (2026-07-28 basic/versioning + transports/streamable-http).
//
// MODERN: every JSON-RPC request is its own POST to the server's one endpoint,
// carrying its version, identity and capabilities in `params._meta` (no handshake,
// no session) and mirrored into headers: `MCP-Protocol-Version`, `Mcp-Method`,
// `Mcp-Name` (a `tools/call`'s tool), `Mcp-Param-<name>` (each `x-mcp-header`
// argument). The answer is one JSON object or an SSE stream scoped to the request
// (its notifications, then the response).
//
// DETECTION (`connect`): `server/discover` is sent modern. A success naming
// 2026-07-28 stays modern. A recognized modern error (`-32022`, `-32021`, `-32020`)
// is a MODERN server: `-32022` (or a discover) listing only legacy versions goes
// legacy, anything else is refused. Any other 4xx (or an error body that is not a
// recognized modern one — `-32601` included) is a LEGACY server → `initialize`.
//
// LEGACY (2025-11-25 Streamable HTTP): `initialize {protocolVersion: 2025-11-25,
// capabilities: {}, clientInfo}` → the server's version (one of `legacy`, else
// refused) and maybe an `MCP-Session-Id`; then `notifications/initialized` (202).
// Every later POST carries `MCP-Session-Id` (when one was given) and
// `MCP-Protocol-Version` (from 2025-06-18 on); no `_meta`, no mirrored headers. A
// 404 on a session is an expired session: one new `initialize`, then the request
// again. A server REQUEST on an SSE stream is answered (a `ping` with `{}`, anything
// else `-32601`). A new session ends the old one (HTTP DELETE; a 405 is fine). The
// deprecated HTTP+SSE transport (2024-11-05's two endpoints) is not spoken.
//
// Every failure is a reply the caller reads: `McpError.reply` is a refusal from
// the kernel's closed set, or `{error, mcp_error}` for a JSON-RPC error the
// server raised.

import AsyncHTTPClient
import FantasticAICore
import FantasticJSON
import FantasticKernel
import Foundation
import NIOCore
import OrderedCollections

public enum McpWire {
    /// The modern protocol: per-request `_meta`, no handshake.
    public static let protocolVersion = "2026-07-28"
    /// The `initialize`-era protocols this client speaks over Streamable HTTP, newest first.
    public static let legacy = ["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"]
    /// Legacy revisions from this one on define the `MCP-Protocol-Version` header.
    public static let versionHeaderFrom = "2025-06-18"
    /// Every protocol this client speaks, as an error names them.
    public static var spoken: String { ([protocolVersion] + legacy).joined(separator: ", ") }
    /// The modern JSON-RPC errors that identify a modern server (never a fallback).
    static let modernErrors: Set<Int64> = [-32022, -32021, -32020]
    /// How this client names itself (`io.modelcontextprotocol/clientInfo`).
    public static let clientName = "aisixteen-fantastic"
    public static let clientVersion = "0.1.0"
    static let safeInt: Int64 = (1 << 53) - 1
    static let tchar = Set("!#$%&'*+-.^_`|~")
    nonisolated(unsafe) static var counter: Int64 = 0
    static let counterLock = NSLock()
    /// A connect that does not happen fails (15 s, python `CONNECT_TIMEOUT`); a
    /// call itself is bounded by its caller's own item timeout, so there is no
    /// read clock. A REFUSED connect is reported at once, as python's httpx does
    /// — the pool would otherwise retry it until the call's deadline.
    static let client: HTTPClient = {
        var cfg = HTTPClient.Configuration(timeout: .init(connect: .seconds(15)))
        cfg.connectionPool.retryConnectionEstablishment = false
        // Network.framework (macOS) otherwise WAITS for connectivity on a refused
        // connect until the connect timeout
        cfg.networkFrameworkWaitForConnectivity = false
        return HTTPClient(eventLoopGroupProvider: .singleton, configuration: cfg)
    }()

    static func nextId() -> String {
        counterLock.lock()
        defer { counterLock.unlock() }
        counter += 1
        return String(counter)
    }
}

/// A call that did not produce a result — `reply` is what the caller gets.
public struct McpError: Error, Sendable {
    public let reply: JSON
    public init(_ reply: JSON) { self.reply = reply }
}

/// Which era a server was found to speak.
public enum McpEra: Sendable, Equatable {
    case modern, legacy
}

/// What one agent agreed with its server: the era, the protocol version, and a
/// legacy server's session id (nil when it gave none).
public struct McpSession: Sendable {
    public var era: McpEra? = nil
    public var version: String? = nil
    public var sessionId: String? = nil
    public init() {}
}

/// Why one exchange did not settle: a reply, or a cue the era logic acts on.
enum McpFault: Error {
    case mcp(McpError)
    /// The modern probe was answered the way only a legacy server answers.
    case legacy
    /// HTTP 404 on a request carrying a session id — the session is gone.
    case expired
}

// ─── header values ───────────────────────────────────────────────────

extension McpWire {
    /// A value as an MCP header carries it: as is when it is plain visible ASCII
    /// (spaces inside), else the `=?base64?…?=` sentinel of its UTF-8 — and a
    /// plain value shaped like the sentinel is encoded too, so it cannot be misread.
    public static func headerValue(_ v: String) -> String {
        let trimmed = v.trimmingCharacters(in: CharacterSet(charactersIn: " \t"))
        let plain =
            v == trimmed
            && v.unicodeScalars.allSatisfy { $0 == " " || $0 == "\t" || ("!"..."~").contains($0) }
            && !(v.hasPrefix("=?base64?") && v.hasSuffix("?="))
        if plain { return v }
        return "=?base64?" + Data(v.utf8).base64EncodedString() + "?="
    }

    /// One mirrored argument: the header name and the `properties` path to it.
    public typealias Mirror = (name: String, path: [String])

    /// The `x-mcp-header` annotations of a tool's `inputSchema` →
    /// `[(name, path)]`, or why the tool must be excluded. Legal only on a
    /// primitive property (integer/string/boolean) reached through `properties`
    /// keys alone; names are non-empty RFC 9110 tokens, unique ignoring case.
    public static func mirrors(_ schema: JSON) -> Result<[Mirror], McpReason> {
        var found: [(JSON, [String])] = []
        func walk(_ node: JSON, _ path: [String]) {
            guard let props = node["properties"].asObject else { return }
            for (key, sub) in props {
                if let h = sub.asObject?["x-mcp-header"] { found.append((h, path + [key])) }
                walk(sub, path + [key])
            }
        }
        func countAll(_ node: JSON) -> Int {
            switch node {
            case .object(let m):
                return (m["x-mcp-header"] == nil ? 0 : 1) + m.values.map(countAll).reduce(0, +)
            case .array(let a): return a.map(countAll).reduce(0, +)
            default: return 0
            }
        }
        walk(schema, [])
        if countAll(schema) != found.count {
            return .failure(McpReason("an x-mcp-header outside a chain of `properties` keys"))
        }
        var seen = Set<String>()
        var out: [Mirror] = []
        for (name, path) in found {
            guard case .string(let n) = name, !n.isEmpty,
                n.allSatisfy({ ($0.isASCII && ($0.isLetter || $0.isNumber)) || tchar.contains($0) })
            else {
                return .failure(
                    McpReason("x-mcp-header \(pyRepr(name)) is not an HTTP header token"))
            }
            if !seen.insert(n.lowercased()).inserted {
                return .failure(McpReason("x-mcp-header \(pyRepr(n)) is not unique"))
            }
            var node = schema
            for key in path { node = node["properties"][key] }
            let ty = node["type"]
            guard case .string(let t) = ty, ["integer", "string", "boolean"].contains(t) else {
                return .failure(
                    McpReason(
                        "x-mcp-header \(pyRepr(n)) is on a \(pyRepr(ty)) property — only integer, string or boolean"
                    ))
            }
            out.append((n, path))
        }
        return .success(out)
    }

    /// `Mcp-Param-<name>` for each mirrored argument present (and not null).
    public static func paramHeaders(_ found: [Mirror], _ arguments: JSON) throws(McpError) -> [(
        String, String
    )] {
        var out: [(String, String)] = []
        for (name, path) in found {
            var node = arguments
            for key in path { node = node.asObject?[key] ?? .null }
            let text: String
            switch node {
            case .null: continue
            case .bool(let b): text = b ? "true" : "false"
            case .integer(let i):
                if abs(i) > safeInt {
                    throw McpError(
                        Refusal.make(
                            Refusal.badArg,
                            "\(path.joined(separator: "."))=\(i) is outside the integers an MCP header carries",
                            extra: [("arg", .string(path[0]))]))
                }
                text = String(i)
            default: text = pyStr(node)
            }
            out.append(("Mcp-Param-\(name)", headerValue(text)))
        }
        return out
    }
}

/// Why a tool was excluded.
public struct McpReason: Error, Sendable, Equatable {
    public let text: String
    public init(_ text: String) { self.text = text }
}

// ─── one request ─────────────────────────────────────────────────────

extension McpWire {
    static func meta() -> JSON {
        .object([
            "io.modelcontextprotocol/protocolVersion": .string(protocolVersion),
            "io.modelcontextprotocol/clientInfo": .object([
                "name": .string(clientName), "version": .string(clientVersion),
            ]),
            "io.modelcontextprotocol/clientCapabilities": .object([:]),
        ])
    }

    static func coded(_ code: String, _ error: String, _ mcpCode: JSON, hint: String = "")
        -> McpError
    {
        McpError(Refusal.make(code, error, hint: hint, extra: [("mcp_code", mcpCode)]))
    }

    /// A JSON-RPC error the server answered, as the caller reads it.
    static func protocolError(_ url: String, _ err: JSON) -> McpError {
        guard err.asObject != nil else {
            return McpError(
                Refusal.make(Refusal.transportError, "mcp \(url): a malformed error \(pyRepr(err))")
            )
        }
        let code = err["code"]
        let msg = err["message"] == .null ? "" : pyStr(err["message"])
        let data = err["data"]
        switch code.asInt {
        case -32602: return coded(Refusal.badArg, "mcp \(url): \(msg)", code)
        case -32601: return coded(Refusal.unhandled, "mcp \(url): \(msg)", code)
        case -32022:
            return McpError(
                Refusal.make(
                    Refusal.transportError,
                    "mcp \(url): the server speaks \(pyRepr(data["supported"])) — this client speaks \(spoken)",
                    extra: [("mcp_code", code), ("supported", data["supported"])]))
        case -32021:
            return coded(
                Refusal.transportError,
                "mcp \(url): the server needs client capabilities \(pyRepr(data["requiredCapabilities"])) this client does not offer",
                code)
        case -32020:
            return coded(
                Refusal.transportError, "mcp \(url): header mismatch — \(msg)", code,
                hint: "the tool's inputSchema may have changed: boot re-lists the tools")
        default:
            return McpError(
                .object([
                    "error": .string("mcp \(url): error \(pyRepr(code)): \(msg)"), "mcp_error": err,
                ]))
        }
    }

    static func settle(_ url: String, _ msg: JSON) throws(McpError) -> OrderedDictionary<
        String, JSON
    > {
        guard msg.asObject != nil else {
            throw McpError(
                Refusal.make(Refusal.transportError, "mcp \(url): not a JSON-RPC message"))
        }
        if let err = msg.asObject?["error"] { throw protocolError(url, err) }
        guard let result = msg["result"].asObject else {
            throw McpError(
                Refusal.make(Refusal.transportError, "mcp \(url): a response with no result"))
        }
        let rtype = result["resultType"] ?? .string("complete")
        guard case .string(let t) = rtype, ["complete", "input_required"].contains(t) else {
            throw McpError(
                Refusal.make(
                    Refusal.transportError, "mcp \(url): unknown resultType \(pyRepr(rtype))"))
        }
        return result
    }

    static let baseHeaders: [(String, String)] = [
        ("Accept", "application/json, text/event-stream"), ("Content-Type", "application/json"),
    ]

    static func legacyHeaders(_ sess: McpSession) -> [(String, String)] {
        var h = baseHeaders
        if let v = sess.version, v >= versionHeaderFrom { h.append(("MCP-Protocol-Version", v)) }
        if let sid = sess.sessionId { h.append(("MCP-Session-Id", sid)) }
        return h
    }

    /// Detect the server's era and agree a version (the file header) — fills `sess`;
    /// → `{server, instructions, capabilities}`. A legacy session `sess` held is ended
    /// first (DELETE).
    public static func connect(_ url: String, key: String?, _ sess: inout McpSession)
        async throws(McpError) -> OrderedDictionary<String, JSON>
    {
        let old = sess.sessionId
        sess = McpSession()
        if let old { await endSession(url, key: key, old) }
        let disc: OrderedDictionary<String, JSON>
        do {
            disc = try await modern(url, "server/discover", [:], key: key, headers: [], probe: true) { _ in }
        } catch {
            switch error {
            case .legacy: return try await initialize(url, key: key, &sess)
            case .mcp(let e):
                if let sup = e.reply["supported"].asArray,
                    sup.contains(where: { legacy.contains($0.asString ?? "") })
                {
                    return try await initialize(url, key: key, &sess)
                }
                throw e
            case .expired: fatalError("a modern request carries no session")
            }
        }
        let versions = disc["supportedVersions"] ?? .array([])
        let names = (versions.asArray ?? []).compactMap(\.asString)
        if !names.contains(protocolVersion) {
            if names.contains(where: { legacy.contains($0) }) {
                return try await initialize(url, key: key, &sess)
            }
            throw McpError(
                Refusal.make(
                    Refusal.transportError,
                    "mcp \(url): the server speaks \(pyRepr(versions)) — this client speaks \(spoken)"))
        }
        sess.era = .modern
        sess.version = protocolVersion
        return [
            "server": disc["_meta"]?["io.modelcontextprotocol/serverInfo"] ?? .null,
            "instructions": disc["instructions"] ?? .null,
            "capabilities": disc["capabilities"] ?? .object([:]),
        ]
    }

    /// POST one request in the agreed era; → its `result`, or `McpError`.
    /// Notifications on an SSE answer go to `onNote` as they arrive. `headers` (the
    /// mirrored `Mcp-Param-*`) ride a modern request only.
    public static func rpc(
        _ url: String, _ method: String, _ params: OrderedDictionary<String, JSON>,
        key: String?, session sess: inout McpSession, headers: [(String, String)] = [],
        onNote: @Sendable (JSON) async -> Void = { _ in }
    ) async throws(McpError) -> OrderedDictionary<String, JSON> {
        if sess.era != .legacy {
            do {
                return try await modern(url, method, params, key: key, headers: headers, probe: false, onNote)
            } catch {
                if case .mcp(let e) = error { throw e }
                fatalError("only a probe falls back")
            }
        }
        do {
            return try await legacyCall(url, method, params, key: key, sess, onNote)
        } catch {
            switch error {
            case .mcp(let e): throw e
            case .legacy: fatalError("only a probe falls back")
            case .expired:
                _ = try await initialize(url, key: key, &sess)
                do {
                    return try await legacyCall(url, method, params, key: key, sess, onNote)
                } catch {
                    if case .mcp(let e) = error { throw e }
                    throw McpError(
                        Refusal.make(
                            Refusal.transportError, "mcp \(url): the new session expired at once"))
                }
            }
        }
    }

    static func modern(
        _ url: String, _ method: String, _ params: OrderedDictionary<String, JSON>,
        key: String?, headers: [(String, String)], probe: Bool,
        _ onNote: @Sendable (JSON) async -> Void
    ) async throws(McpFault) -> OrderedDictionary<String, JSON> {
        let rid = nextId()
        var p = params
        p["_meta"] = meta()
        let body: JSON = .object([
            "jsonrpc": .string("2.0"), "id": .string(rid), "method": .string(method),
            "params": .object(p),
        ])
        var h = baseHeaders
        h.append(("MCP-Protocol-Version", protocolVersion))
        h.append(("Mcp-Method", method))
        if method == "tools/call" { h.append(("Mcp-Name", headerValue(p["name"]?.asString ?? ""))) }
        h += headers
        return try await post(url, body, h, key: key, rid: rid, probe: probe, sess: nil, onNote).0
    }

    static func legacyCall(
        _ url: String, _ method: String, _ params: OrderedDictionary<String, JSON>,
        key: String?, _ sess: McpSession, _ onNote: @Sendable (JSON) async -> Void
    ) async throws(McpFault) -> OrderedDictionary<String, JSON> {
        let rid = nextId()
        let body: JSON = .object([
            "jsonrpc": .string("2.0"), "id": .string(rid), "method": .string(method),
            "params": .object(params),
        ])
        return try await post(url, body, legacyHeaders(sess), key: key, rid: rid, probe: false, sess: sess, onNote).0
    }

    /// The legacy handshake: `initialize` at the newest legacy version, the server's
    /// answer (its version must be one this client speaks), its session id, then
    /// `notifications/initialized`.
    static func initialize(_ url: String, key: String?, _ sess: inout McpSession)
        async throws(McpError) -> OrderedDictionary<String, JSON>
    {
        sess = McpSession()
        sess.era = .legacy
        let rid = nextId()
        let body: JSON = .object([
            "jsonrpc": "2.0", "id": .string(rid), "method": "initialize",
            "params": [
                "protocolVersion": .string(legacy[0]), "capabilities": .object([:]),
                "clientInfo": ["name": .string(clientName), "version": .string(clientVersion)],
            ],
        ])
        let result: OrderedDictionary<String, JSON>
        let head: [(String, String)]
        do {
            (result, head) = try await post(url, body, baseHeaders, key: key, rid: rid, probe: false, sess: nil) { _ in }
        } catch {
            if case .mcp(let e) = error { throw e }
            fatalError("no probe, no session")
        }
        let version = result["protocolVersion"] ?? .null
        guard let v = version.asString, legacy.contains(v) else {
            sess.era = nil
            throw McpError(
                Refusal.make(
                    Refusal.transportError,
                    "mcp \(url): the server answered initialize with protocol \(pyRepr(version)) — this client speaks \(spoken)"))
        }
        sess.version = v
        let sid = head.first { $0.0.lowercased() == "mcp-session-id" }?.1
        sess.sessionId = (sid?.isEmpty ?? true) ? nil : sid
        try await sendMessage(url, ["jsonrpc": "2.0", "method": "notifications/initialized"], key: key, sess)
        return [
            "server": result["serverInfo"] ?? .null,
            "instructions": result["instructions"] ?? .null,
            "capabilities": result["capabilities"] ?? .object([:]),
        ]
    }

    /// POST a notification or a response (legacy): the server answers 202 Accepted.
    static func sendMessage(_ url: String, _ msg: JSON, key: String?, _ sess: McpSession)
        async throws(McpError)
    {
        var req = HTTPClientRequest(url: url)
        req.method = .POST
        for (k, v) in legacyHeaders(sess) { req.headers.add(name: k, value: v) }
        if let key { req.headers.add(name: "Authorization", value: "Bearer \(key)") }
        req.body = .bytes(ByteBuffer(string: msg.serialize()))
        let resp: HTTPClientResponse
        do {
            resp = try await client.execute(req, deadline: .distantFuture)
        } catch {
            throw McpError(Refusal.make(Refusal.notConnected, "mcp \(url): cannot connect: \(error)"))
        }
        let status = Int(resp.status.code)
        let raw = (try? await resp.body.collect(upTo: 1 << 20)).map { Data(buffer: $0) } ?? Data()
        if status >= 300 {
            let what =
                msg["method"].asString ?? "the response to \(pyRepr(msg["id"]))"
            let text = String(String(decoding: raw, as: UTF8.self).prefix(200))
            throw McpError(
                Refusal.make(
                    Refusal.transportError,
                    "mcp \(url): \(what) answered HTTP \(status) (202 expected): \(text)",
                    extra: [("http_status", .integer(Int64(status)))]))
        }
    }

    /// End a legacy session (HTTP DELETE) — best effort: a server may answer 405, or
    /// be gone.
    static func endSession(_ url: String, key: String?, _ sid: String) async {
        var req = HTTPClientRequest(url: url)
        req.method = .DELETE
        req.headers.add(name: "MCP-Session-Id", value: sid)
        if let key { req.headers.add(name: "Authorization", value: "Bearer \(key)") }
        if let resp = try? await client.execute(req, deadline: .distantFuture) {
            _ = try? await resp.body.collect(upTo: 1 << 20)
        }
    }

    /// A probe's error body is NOT a recognized modern error — a legacy server.
    static func legacyAnswer(_ err: JSON) -> Bool {
        guard let c = err["code"].asInt else { return true }
        return !modernErrors.contains(c)
    }

    /// A legacy server's own REQUEST on an SSE stream: a `ping` is answered `{}`;
    /// anything else `-32601` — this client declares no capability.
    static func answerServerRequest(_ url: String, _ msg: JSON, key: String?, _ sess: McpSession)
        async throws(McpError)
    {
        let reply: JSON
        if msg["method"].asString == "ping" {
            reply = .object(["jsonrpc": "2.0", "id": msg["id"], "result": .object([:])])
        } else {
            reply = .object([
                "jsonrpc": "2.0", "id": msg["id"],
                "error": [
                    "code": .integer(-32601),
                    "message": .string("this client does not serve \(pyRepr(msg["method"]))"),
                ],
            ])
        }
        try await sendMessage(url, reply, key: key, sess)
    }

    /// One request POSTed; → (its result, the response headers).
    static func post(
        _ url: String, _ body: JSON, _ headers: [(String, String)], key: String?, rid: String,
        probe: Bool, sess: McpSession?, _ onNote: @Sendable (JSON) async -> Void
    ) async throws(McpFault) -> (OrderedDictionary<String, JSON>, [(String, String)]) {
        var req = HTTPClientRequest(url: url)
        req.method = .POST
        for (k, v) in headers { req.headers.add(name: k, value: v) }
        if let key { req.headers.add(name: "Authorization", value: "Bearer \(key)") }
        req.body = .bytes(ByteBuffer(string: body.serialize()))
        let response: HTTPClientResponse
        do {
            response = try await client.execute(req, deadline: .distantFuture)
        } catch {
            throw .mcp(
                McpError(Refusal.make(Refusal.notConnected, "mcp \(url): cannot connect: \(error)")))
        }
        let head = response.headers.map { ($0.name, $0.value) }
        let result = try await read(url, rid, response, key: key, probe: probe, sess: sess, onNote)
        return (result, head)
    }

    static func read(
        _ url: String, _ rid: String, _ resp: HTTPClientResponse, key: String?, probe: Bool,
        sess: McpSession?, _ onNote: @Sendable (JSON) async -> Void
    ) async throws(McpFault) -> OrderedDictionary<String, JSON> {
        func fail(_ r: JSON) -> McpFault { .mcp(McpError(r)) }
        let status = Int(resp.status.code)
        if status == 401 || status == 403 {
            let why = key != nil ? "refused the key" : "needs a key"
            throw fail(
                Refusal.make(
                    Refusal.unauthorized, "mcp \(url): HTTP \(status) — the server \(why)",
                    hint:
                        "the key is a credential-store ref: credentials={store, ref}, field api_key",
                    extra: [("http_status", .integer(Int64(status)))]))
        }
        let ctype =
            (resp.headers.first(name: "content-type") ?? "")
            .split(separator: ";").first.map {
                $0.trimmingCharacters(in: .whitespaces).lowercased()
            } ?? ""
        func collect() async throws(McpFault) -> Data {
            do {
                let buf = try await resp.body.collect(upTo: 64 << 20)
                return Data(buffer: buf)
            } catch {
                throw fail(Refusal.make(Refusal.transportError, "mcp \(url): \(error)"))
            }
        }
        func settled(_ msg: JSON) throws(McpFault) -> OrderedDictionary<String, JSON> {
            do { return try settle(url, msg) } catch { throw .mcp(error) }
        }
        if status == 404, sess?.sessionId != nil {
            _ = try? await resp.body.collect(upTo: 1 << 20)
            throw .expired
        }
        if status >= 400 {
            let raw = try await collect()
            let msg = try? JSON.parse(raw)
            let err = msg?["error"] ?? .null
            if probe && status < 500 && legacyAnswer(err) { throw .legacy }
            if err.asObject != nil { throw .mcp(protocolError(url, err)) }
            let text = String(String(decoding: raw, as: UTF8.self).prefix(300))
            throw fail(
                Refusal.make(
                    Refusal.transportError, "mcp \(url): HTTP \(status): \(text)",
                    extra: [("http_status", .integer(Int64(status)))]))
        }
        if ctype == "application/json" {
            let raw = try await collect()
            let msg: JSON
            do { msg = try JSON.parse(raw) } catch {
                throw fail(
                    Refusal.make(
                        Refusal.transportError, "mcp \(url): the answer is not JSON: \(error)"))
            }
            if probe, msg.asObject?["error"] != nil, legacyAnswer(msg["error"]) { throw .legacy }
            return try settled(msg)
        }
        if ctype == "text/event-stream" {
            var data: [String] = []
            do {
                for try await raw in bytesToLines(resp.body) {
                    let line = raw.hasSuffix("\r") ? String(raw.dropLast()) : raw
                    if line.hasPrefix(":") { continue }  // an SSE comment (keep-alive)
                    if line.hasPrefix("data:") {
                        var rest = String(line.dropFirst(5))
                        if rest.hasPrefix(" ") { rest.removeFirst() }
                        data.append(rest)
                        continue
                    }
                    if line.isEmpty && !data.isEmpty {
                        let event = data.joined(separator: "\n")
                        data = []
                        let msg: JSON
                        do { msg = try JSON.parse(event) } catch {
                            throw fail(
                                Refusal.make(
                                    Refusal.transportError,
                                    "mcp \(url): an SSE event is not JSON: \(error)"))
                        }
                        if let m = msg.asObject, m["id"] == nil, m["method"] != nil {
                            await onNote(msg)
                            continue
                        }
                        if let s = sess, msg.asObject?["method"] != nil {
                            do { try await answerServerRequest(url, msg, key: key, s) } catch {
                                throw McpFault.mcp(error)
                            }
                            continue
                        }
                        if msg["id"] == .string(rid) {
                            if probe, msg.asObject?["error"] != nil, legacyAnswer(msg["error"]) {
                                throw McpFault.legacy
                            }
                            return try settled(msg)
                        }
                    }
                }
            } catch let e as McpFault {
                throw e
            } catch {
                throw fail(Refusal.make(Refusal.transportError, "mcp \(url): \(error)"))
            }
            throw fail(
                Refusal.make(
                    Refusal.transportClosed, "mcp \(url): the stream ended before the response"))
        }
        throw fail(
            Refusal.make(
                Refusal.transportError,
                "mcp \(url): unexpected content-type \(pyRepr(ctype.isEmpty ? "none" : ctype))"))
    }
}
