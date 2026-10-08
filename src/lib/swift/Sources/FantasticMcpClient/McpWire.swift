// The MCP wire — Streamable HTTP, protocol 2026-07-28 ONLY (owner, 2026-10-08).
// Python `mcp_client/wire.py`, text for text.
//
// Every JSON-RPC request is its own POST to the server's one endpoint, carrying its
// version, identity and capabilities in `params._meta` (no handshake, no session)
// and mirrored into headers: `MCP-Protocol-Version`, `Mcp-Method`, `Mcp-Name` (a
// `tools/call`'s tool), `Mcp-Param-<name>` (each `x-mcp-header` argument). The
// answer is one JSON object or an SSE stream scoped to the request (its
// notifications, then the response). A server on an older protocol is refused
// naming its versions — no handshake fallback.
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
    /// The one protocol version this client speaks.
    public static let protocolVersion = "2026-07-28"
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
    /// — the pool would otherwise retry it until the call's deadline (measured:
    /// 90 s a request).
    static let client: HTTPClient = {
        var cfg = HTTPClient.Configuration(timeout: .init(connect: .seconds(15)))
        cfg.connectionPool.retryConnectionEstablishment = false
        // Network.framework (macOS) otherwise WAITS for connectivity on a refused
        // connect until the connect timeout (measured: 15 s a request)
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
            return coded(
                Refusal.transportError,
                "mcp \(url): the server speaks \(pyRepr(data["supported"])) — this client speaks \(protocolVersion) only",
                code)
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

    /// POST one request; → its `result`, or `McpError`. Notifications on an SSE
    /// answer go to `onNote` as they arrive.
    public static func rpc(
        _ url: String, _ method: String, _ params: OrderedDictionary<String, JSON>,
        key: String?, headers: [(String, String)] = [],
        onNote: @Sendable (JSON) async -> Void = { _ in }
    ) async throws(McpError) -> OrderedDictionary<String, JSON> {
        let rid = nextId()
        var p = params
        p["_meta"] = meta()
        let body: JSON = .object([
            "jsonrpc": .string("2.0"), "id": .string(rid), "method": .string(method),
            "params": .object(p),
        ])
        var req = HTTPClientRequest(url: url)
        req.method = .POST
        req.headers.add(name: "Accept", value: "application/json, text/event-stream")
        req.headers.add(name: "Content-Type", value: "application/json")
        req.headers.add(name: "MCP-Protocol-Version", value: protocolVersion)
        req.headers.add(name: "Mcp-Method", value: method)
        if method == "tools/call" {
            req.headers.add(name: "Mcp-Name", value: headerValue(p["name"]?.asString ?? ""))
        }
        for (k, v) in headers { req.headers.add(name: k, value: v) }
        if let key { req.headers.add(name: "Authorization", value: "Bearer \(key)") }
        req.body = .bytes(ByteBuffer(string: body.serialize()))
        let response: HTTPClientResponse
        do {
            response = try await client.execute(req, deadline: .distantFuture)
        } catch {
            throw McpError(
                Refusal.make(Refusal.notConnected, "mcp \(url): cannot connect: \(error)"))
        }
        return try await read(url, rid, response, keyed: key != nil, onNote)
    }

    static func read(
        _ url: String, _ rid: String, _ resp: HTTPClientResponse, keyed: Bool,
        _ onNote: @Sendable (JSON) async -> Void
    ) async throws(McpError) -> OrderedDictionary<String, JSON> {
        let status = Int(resp.status.code)
        if status == 401 || status == 403 {
            let why = keyed ? "refused the key" : "needs a key"
            throw McpError(
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
        func collect() async throws(McpError) -> Data {
            do {
                let buf = try await resp.body.collect(upTo: 64 << 20)
                return Data(buffer: buf)
            } catch {
                throw McpError(Refusal.make(Refusal.transportError, "mcp \(url): \(error)"))
            }
        }
        if status >= 400 {
            let raw = try await collect()
            if let msg = try? JSON.parse(raw), msg["error"].asObject != nil {
                throw protocolError(url, msg["error"])
            }
            let text = String(String(decoding: raw, as: UTF8.self).prefix(300))
            throw McpError(
                Refusal.make(
                    Refusal.transportError, "mcp \(url): HTTP \(status): \(text)",
                    extra: [("http_status", .integer(Int64(status)))]))
        }
        if ctype == "application/json" {
            let raw = try await collect()
            let msg: JSON
            do { msg = try JSON.parse(raw) } catch {
                throw McpError(
                    Refusal.make(
                        Refusal.transportError, "mcp \(url): the answer is not JSON: \(error)"))
            }
            return try settle(url, msg)
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
                            throw McpError(
                                Refusal.make(
                                    Refusal.transportError,
                                    "mcp \(url): an SSE event is not JSON: \(error)"))
                        }
                        if let m = msg.asObject, m["id"] == nil, m["method"] != nil {
                            await onNote(msg)
                            continue
                        }
                        if msg["id"] == .string(rid) { return try settle(url, msg) }
                    }
                }
            } catch let e as McpError {
                throw e
            } catch {
                throw McpError(Refusal.make(Refusal.transportError, "mcp \(url): \(error)"))
            }
            throw McpError(
                Refusal.make(
                    Refusal.transportClosed, "mcp \(url): the stream ended before the response"))
        }
        throw McpError(
            Refusal.make(
                Refusal.transportError,
                "mcp \(url): unexpected content-type \(pyRepr(ctype.isEmpty ? "none" : ctype))"))
    }
}
