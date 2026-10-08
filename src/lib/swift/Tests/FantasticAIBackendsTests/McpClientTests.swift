// mcp_client — python `mcp_client/tests/test_mcp_client.py`, case for case:
// against a fake 2026-07-28 server (a real loopback listener, `StubHTTP`) that
// enforces the spec's request rules (headers mirror the body, `_meta` on every
// request, the Bearer key), through a real credential store (file backend under a
// temp dir).

import FantasticAICore
import FantasticJSON
@testable import FantasticKernel
import Foundation
import OrderedCollections
import Testing

@testable import FantasticCredentialStore
@testable import FantasticMcpClient

private let KEY = "mcp-key-0123456789"
private let PNG = Data([
    0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x01, 0x72, 0x61, 0x77,
])

private let TOOLS: [JSON] = {
    let text = """
        [
         {"name": "get_weather", "title": "Weather",
          "description": "Current weather for a city. Units default to metric.",
          "inputSchema": {"type": "object", "properties": {
             "city": {"type": "string"}, "days": {"type": "integer"},
             "region": {"type": "string", "x-mcp-header": "Region"}},
             "required": ["city"]}},
         {"name": "fail", "description": "Always a tool error.", "inputSchema": {"type": "object"}},
         {"name": "picture", "inputSchema": {"type": "object", "additionalProperties": false}},
         {"name": "stream", "description": "Answers over SSE with progress.", "inputSchema": {"type": "object"}},
         {"name": "structured", "inputSchema": {"type": "object"}},
         {"name": "ask", "inputSchema": {"type": "object"}},
         {"name": "weird", "inputSchema": {"type": "object"}},
         {"name": "cut", "inputSchema": {"type": "object"}},
         {"name": "reflect", "inputSchema": {"type": "object"}},
         {"name": "ps", "inputSchema": {"type": "object"}},
         {"name": "fail", "inputSchema": {"type": "object"}},
         {"name": "bad_header", "inputSchema": {"type": "object", "properties": {
            "xs": {"type": "array", "items": {"type": "string", "x-mcp-header": "X"}}}}}
        ]
        """
    return (try? JSON.parse(text))?.asArray ?? []
}()

/// The keychain is never reached here (file backend).
private struct NoKeychain: KeychainStore {
    func get(service: String, account: String) throws -> String? { nil }
    func set(service: String, account: String, value: String) throws {}
    func delete(service: String, account: String) throws {}
}

/// One request as the fake read it.
private struct Seen {
    let headers: [String: String]
    let body: JSON
}

/// The request text → its headers (names lowercased) and JSON body.
private func parse(_ request: String) -> Seen {
    let parts = request.components(separatedBy: "\r\n\r\n")
    var headers: [String: String] = [:]
    for line in parts[0].components(separatedBy: "\r\n").dropFirst() {
        guard let colon = line.firstIndex(of: ":") else { continue }
        headers[line[..<colon].lowercased()] = line[line.index(after: colon)...]
            .trimmingCharacters(in: .whitespaces)
    }
    let body = (try? JSON.parse(parts.dropFirst().joined(separator: "\r\n\r\n"))) ?? .null
    return Seen(headers: headers, body: body)
}

private func reply(_ status: Int, _ obj: JSON) -> StubHTTP.Reply {
    StubHTTP.Reply(
        status: status, headers: [("Content-Type", "application/json")], body: obj.serialize())
}

private func ok(_ body: JSON, _ result: OrderedDictionary<String, JSON>) -> StubHTTP.Reply {
    var r: OrderedDictionary<String, JSON> = ["resultType": "complete"]
    for (k, v) in result { r[k] = v }
    return reply(200, .object(["jsonrpc": "2.0", "id": body["id"], "result": .object(r)]))
}

private func err(_ status: Int, _ body: JSON, _ code: Int64, _ msg: String) -> StubHTTP.Reply {
    reply(
        status,
        .object([
            "jsonrpc": "2.0", "id": body["id"],
            "error": ["code": .integer(code), "message": .string(msg)],
        ]))
}

/// A 2026-07-28 server: validates what the spec makes it validate, answers by tool.
private func fakeMcp(key: String? = KEY, versions: [String] = ["2026-07-28"]) -> StubHTTP {
    StubHTTP(respond: { request in
        let r = parse(request)
        let h = r.headers
        let body = r.body
        if let key, h["authorization"] != "Bearer \(key)" {
            return StubHTTP.Reply(status: 401, headers: [("WWW-Authenticate", "Bearer")])
        }
        let meta = body["params"]["_meta"]
        if meta["io.modelcontextprotocol/protocolVersion"] == .null
            || meta["io.modelcontextprotocol/clientCapabilities"] == .null
        {
            return err(400, body, -32602, "missing _meta")
        }
        if h["mcp-protocol-version"] != meta["io.modelcontextprotocol/protocolVersion"].asString {
            return err(400, body, -32020, "protocol header mismatch")
        }
        if h["mcp-method"] != body["method"].asString {
            return err(400, body, -32020, "Mcp-Method mismatch")
        }
        let accept = h["accept"] ?? ""
        if !accept.contains("application/json") || !accept.contains("text/event-stream") {
            return StubHTTP.Reply(status: 406)
        }
        switch body["method"].asString ?? "" {
        case "server/discover":
            return ok(
                body,
                [
                    "supportedVersions": .array(versions.map { .string($0) }),
                    "capabilities": ["tools": ["listChanged": true]],
                    "_meta": [
                        "io.modelcontextprotocol/serverInfo": ["name": "fake", "version": "9"]
                    ],
                    "instructions": "Weather and friends.",
                ])
        case "tools/list":
            let start = Int(body["params"]["cursor"].asString ?? "0") ?? 0
            var res: OrderedDictionary<String, JSON> = [
                "tools": .array(Array(TOOLS.dropFirst(start).prefix(3)))
            ]
            if start + 3 < TOOLS.count { res["nextCursor"] = .string(String(start + 3)) }
            return ok(body, res)
        case "tools/call":
            let name = body["params"]["name"].asString ?? ""
            if h["mcp-name"] != name { return err(400, body, -32020, "Mcp-Name mismatch") }
            return tool(name, body, h["mcp-param-region"])
        case let m:
            return err(404, body, -32601, "no method \(m)")
        }
    })
}

private func tool(_ name: String, _ body: JSON, _ regionHeader: String?) -> StubHTTP.Reply {
    let args = body["params"]["arguments"]
    let b64 = PNG.base64EncodedString()
    switch name {
    case "get_weather":
        guard let city = args["city"].asString else {
            return err(400, body, -32602, "city required")
        }
        if let region = args["region"].asString, regionHeader != region {
            return err(400, body, -32020, "Mcp-Param-Region mismatch")
        }
        return ok(
            body,
            ["content": [["type": "text", "text": .string("sunny in \(city)")]], "isError": false])
    case "fail":
        return ok(body, ["content": [["type": "text", "text": "no such city"]], "isError": true])
    case "picture":
        return ok(
            body,
            [
                "content": [
                    ["type": "image", "data": .string(b64), "mimeType": "image/png"],
                    [
                        "type": "resource",
                        "resource": [
                            "uri": "mem://x", "mimeType": "application/octet-stream",
                            "blob": .string(b64),
                        ],
                    ],
                ]
            ])
    case "structured":
        return ok(
            body,
            ["content": [["type": "text", "text": "{\"t\": 21}"]], "structuredContent": ["t": 21]])
    case "ask":
        return ok(
            body,
            [
                "resultType": "input_required",
                "inputRequests": [
                    "who": ["method": "elicitation/create", "params": ["message": "name?"]]
                ],
                "requestState": "st-1",
            ])
    case "weird":
        return ok(body, ["resultType": "telepathy"])
    default:
        var events: [JSON] = []
        if name == "stream" {
            events.append([
                "jsonrpc": "2.0", "method": "notifications/progress",
                "params": ["progressToken": "p", "progress": 1, "total": 2],
            ])
            events.append([
                "jsonrpc": "2.0", "method": "notifications/message",
                "params": ["level": "info", "data": "half"],
            ])
            events.append([
                "jsonrpc": "2.0", "id": body["id"],
                "result": [
                    "resultType": "complete", "content": [["type": "text", "text": "done"]],
                ],
            ])
        }
        if name == "cut" {
            events.append([
                "jsonrpc": "2.0", "method": "notifications/progress",
                "params": ["progressToken": "p", "progress": 1],
            ])
        }
        let sse = ": keep-alive\n\n" + events.map { "data: \($0.serialize())\n\n" }.joined()
        return StubHTTP.Reply.sse(sse)
    }
}

private func send(
    _ k: Kernel, _ target: String, _ payload: OrderedDictionary<String, JSON>,
    timeout: Double? = nil
)
    async -> Outcome
{
    await k.send([Item(AgentId(target), .object(payload), timeout: timeout)])[0]
}

private func create(_ k: Kernel, _ fields: OrderedDictionary<String, JSON>) async -> JSON {
    var payload: OrderedDictionary<String, JSON> = [
        "type": "create_agent", "parent_id": "kernel_state",
    ]
    for (key, v) in fields { payload[key] = v }
    return await k.send([Item("kernel", .object(payload))])[0].result
}

/// A kernel with the store, a key for `host` (nil = the server's own
/// `127.0.0.1:<port>`), and agent `mcp` on `url` — created, so booted once.
private func mk(
    _ url: String, host: String? = nil, key: String = KEY, credentials: Bool = true
) async -> (Kernel, URL) {
    let r = BundleRegistry()
    r.register(CredentialStoreBundle.handlerModule, CredentialStoreBundle(keychain: NoKeychain()))
    r.register(MCP_CLIENT_HANDLER_MODULE, McpClientBundle())
    let k = Kernel(storage: .inMemory, bundles: r)
    let root = Agent(id: AgentId("kernel_state"), handlerModule: nil, parentId: nil)
    k.register(root)
    k.setRoot(root)
    k.update(root.id, ["guid": "g1"])
    let comps = URLComponents(string: url)!
    let authority = "\(comps.host!):\(comps.port!)"
    let dir = FileManager.default.temporaryDirectory.appendingPathComponent(
        "mcp-\(UUID().uuidString.prefix(8))")
    _ = await create(
        k,
        [
            "handler_module": .string(CredentialStoreBundle.handlerModule), "id": "creds",
            "file": .string(dir.appendingPathComponent("creds.yaml").path),
            "refs": ["main": ["host": .string(host ?? authority)]],
            "callable_by": ["mcp": "*"],
        ])
    let stored = await send(
        k, "creds", ["type": "set", "ref": "main", "fields": ["api_key": .string(key)]])
    #expect(stored.result["stored"] == true, "\(stored.result.serialize())")
    var fields: OrderedDictionary<String, JSON> = [
        "handler_module": .string(MCP_CLIENT_HANDLER_MODULE), "id": "mcp", "url": .string(url),
        "can_call": ["creds": "*"],
    ]
    if credentials { fields["credentials"] = ["store": "creds", "ref": "main"] }
    let rec = await create(k, fields)
    #expect(rec["id"] == "mcp", "\(rec.serialize())")
    return (k, dir)
}

private func seen(_ stub: StubHTTP, _ method: String? = nil) -> [Seen] {
    stub.requests.map(parse).filter { method == nil || $0.body["method"].asString == method }
}

@Suite("mcp_client: a remote MCP server as an agent", .serialized)
struct McpClientTests {
    @Test func bootListsEveryPageAndSkipsWhatItCannotServe() async throws {
        let stub = fakeMcp()
        let (k, dir) = await mk("http://127.0.0.1:\(stub.port)/mcp")

        defer { try? FileManager.default.removeItem(at: dir) }
        let out = await send(k, "mcp", ["type": "boot"]).result
        #expect(out["server"] == ["name": "fake", "version": "9"])
        #expect(
            out["tools"]
                == [
                    "get_weather", "fail", "picture", "stream", "structured", "ask", "weird", "cut",
                ])
        let skipped = Dictionary(
            uniqueKeysWithValues: (out["skipped"].asArray ?? []).map {
                ($0["name"].asString ?? "", $0["why"].asString ?? "")
            })
        #expect(Set(skipped.keys) == ["reflect", "ps", "fail", "bad_header"], "\(skipped)")
        #expect(skipped["fail"]?.contains("listed twice") == true)
        #expect(skipped["bad_header"]?.contains("properties") == true)
        // the second boot (create_agent booted it once): every page, by its cursor
        let cursors = seen(stub, "tools/list").suffix(4).map { $0.body["params"]["cursor"] }
        #expect(cursors == [.null, "3", "6", "9"])
        let rf = await send(k, "mcp", ["type": "reflect"]).result
        #expect(rf["protocol_version"] == "2026-07-28" && rf["tools"] == 8)
        #expect(rf["instructions"] == "Weather and friends.")
        #expect(rf["verbs"] == .null, "reflect says what it is, never how to call it")
    }

    @Test func docsServeThisAgentsOwnTableBuiltFromTheSchemas() async throws {
        let stub = fakeMcp()
        let (k, dir) = await mk("http://127.0.0.1:\(stub.port)/mcp")

        defer { try? FileManager.default.removeItem(at: dir) }
        let d = await send(k, "kernel", ["type": "docs", "of": "mcp"]).result
        #expect(d["bundle"] == .string(MCP_CLIENT_HANDLER_MODULE))
        #expect(d["verbs"]["get_weather"] == "args: city:str (req), days:int?, region:str?")
        #expect(d["verbs"]["picture"] == "No args")
        #expect(d["verbs"].asObject?.count == 10, "\(d.serialize())")
        let full = await send(
            k, "kernel", ["type": "docs", "of": "mcp", "tier": "detailed", "verb": "get_weather"]
        ).result
        #expect(
            full["verbs"]["get_weather"].asString?.hasSuffix(
                "Current weather for a city. Units default to metric.") == true)
        let b = await send(k, "kernel", ["type": "docs", "of": "mcp_client"]).result
        #expect(Array(b["verbs"].asObject?.keys ?? []) == ["reflect", "boot"])
    }

    @Test func everyRequestCarriesMetaMirroredHeadersAndTheKey() async throws {
        let stub = fakeMcp()
        let (k, dir) = await mk("http://127.0.0.1:\(stub.port)/mcp")

        defer { try? FileManager.default.removeItem(at: dir) }
        let out = await send(k, "mcp", ["type": "get_weather", "city": "Kyiv", "region": "eu-1"])
        #expect(out.status == .completed, "\(out.result.serialize())")
        #expect(out.result == ["content": [["type": "text", "text": "sunny in Kyiv"]]])
        for s in seen(stub) {
            #expect(s.headers["authorization"] == "Bearer \(KEY)", "the key on EVERY request")
            #expect(s.headers["mcp-protocol-version"] == "2026-07-28")
            #expect(
                s.body["params"]["_meta"]["io.modelcontextprotocol/clientInfo"]["name"]
                    == "aisixteen-fantastic")
        }
        let call = try #require(seen(stub, "tools/call").last)
        #expect(call.body["params"]["arguments"] == ["city": "Kyiv", "region": "eu-1"])
        #expect(call.headers["mcp-name"] == "get_weather")
        #expect(call.headers["mcp-param-region"] == "eu-1")
        _ = await send(k, "mcp", ["type": "get_weather", "city": "Lviv"])
        #expect(seen(stub, "tools/call").last?.headers["mcp-param-region"] == nil)
        let ledger = await send(k, "kernel", ["type": "ledger"]).result.serialize()
        #expect(!ledger.contains(KEY), "the key never lands in the ledger")
    }

    @Test func aRecordWithoutCredentialsSendsNoKey() async throws {
        let stub = fakeMcp(key: nil)
        let (k, dir) = await mk("http://127.0.0.1:\(stub.port)/mcp", credentials: false)

        defer { try? FileManager.default.removeItem(at: dir) }
        #expect(await send(k, "mcp", ["type": "boot"]).status == .completed)
        #expect(seen(stub).allSatisfy { $0.headers["authorization"] == nil })
    }

    @Test func aKeyPinnedToAnotherHostIsNeverSent() async throws {
        let stub = fakeMcp()
        let (k, dir) = await mk("http://127.0.0.1:\(stub.port)/mcp", host: "elsewhere.test")

        defer { try? FileManager.default.removeItem(at: dir) }
        let out = await send(k, "mcp", ["type": "boot"]).result
        #expect(out["reason"] == "unauthorized", "\(out.serialize())")
        #expect(stub.requests.isEmpty, "nothing left for the server")
    }

    @Test func aRefusedKeyIsUnauthorized() async throws {
        let stub = fakeMcp()
        let (k, dir) = await mk("http://127.0.0.1:\(stub.port)/mcp", key: "wrong-key")

        defer { try? FileManager.default.removeItem(at: dir) }
        let out = await send(k, "mcp", ["type": "boot"]).result
        #expect(out["reason"] == "unauthorized" && out["http_status"] == 401, "\(out.serialize())")
    }

    @Test func headerValuesRidePlainOrAsTheBase64Sentinel() throws {
        #expect(McpWire.headerValue("us-west1") == "us-west1")
        #expect(McpWire.headerValue("Hello, 世界") == "=?base64?SGVsbG8sIOS4lueVjA==?=")
        #expect(McpWire.headerValue(" padded ") == "=?base64?IHBhZGRlZCA=?=")
        #expect(McpWire.headerValue("=?base64?literal?=") == "=?base64?PT9iYXNlNjQ/bGl0ZXJhbD89?=")
        let found = try McpWire.mirrors(TOOLS[0]["inputSchema"]).get()
        #expect(found.map(\.name) == ["Region"] && found.map(\.path) == [["region"]])
        let h = try McpWire.paramHeaders(found, ["region": "日本"])
        #expect(h.map(\.0) == ["Mcp-Param-Region"] && h.map(\.1) == ["=?base64?5pel5pys?="])
        let b = try McpWire.paramHeaders([(name: "On", path: ["on"])], ["on": true])
        #expect(b.map(\.1) == ["true"])
        if case .success = McpWire.mirrors([
            "properties": ["n": ["type": "number", "x-mcp-header": "N"]]
        ]) {
            Issue.record("a number property may not be mirrored")
        }
        if case .success = McpWire.mirrors([
            "properties": [
                "a": ["type": "string", "x-mcp-header": "A"],
                "b": ["type": "string", "x-mcp-header": "a"],
            ]
        ]) {
            Issue.record("header names are unique ignoring case")
        }
    }

    @Test func aToolErrorSettlesErroredWithItsText() async throws {
        let stub = fakeMcp()
        let (k, dir) = await mk("http://127.0.0.1:\(stub.port)/mcp")

        defer { try? FileManager.default.removeItem(at: dir) }
        let out = await send(k, "mcp", ["type": "fail"])
        #expect(out.status == .errored, "\(out.result.serialize())")
        #expect(out.result["error"] == "no such city")
        #expect(out.result["content"] == [["type": "text", "text": "no such city"]])
    }

    @Test func binaryContentArrivesAsRawBytes() async throws {
        let stub = fakeMcp()
        let (k, dir) = await mk("http://127.0.0.1:\(stub.port)/mcp")

        defer { try? FileManager.default.removeItem(at: dir) }
        // a binary reply rides a binary item (`rawReply`), as the web legs send it
        let out = await k.send([Item(AgentId("mcp"), ["type": "picture"], rawReply: true)])[0]
        let body = try #require(out.body, "the first binary item is the reply's body")
        // ONE binary body, at the reply's top-level `bytes`; the item names it
        #expect(body.bytes == PNG)
        #expect(body.parts == [.key("bytes")])
        #expect(
            out.result["content"][0]
                == [
                    "type": "image", "mimeType": "image/png", "size": .integer(Int64(PNG.count)),
                    "body": "bytes",
                ])
        #expect(
            out.result["content"][1]["resource"]
                == [
                    "uri": "mem://x", "mimeType": "application/octet-stream",
                    "size": .integer(Int64(PNG.count)),
                    "omitted": .string(MCP_OMITTED),
                ])
    }

    @Test func structuredContentIsKept() async throws {
        let stub = fakeMcp()
        let (k, dir) = await mk("http://127.0.0.1:\(stub.port)/mcp")

        defer { try? FileManager.default.removeItem(at: dir) }
        #expect(await send(k, "mcp", ["type": "structured"]).result["structured"] == ["t": 21])
    }

    @Test func anSseAnswerSettlesAndItsNotesReachTheWatchers() async throws {
        let stub = fakeMcp()
        let (k, dir) = await mk("http://127.0.0.1:\(stub.port)/mcp")

        defer { try? FileManager.default.removeItem(at: dir) }
        let watcher = AgentId("_t_watch")
        let inbox = k.ensureInbox(watcher)
        #expect(k.watch(src: AgentId("mcp"), watcher: watcher) == nil)
        let out = await send(k, "mcp", ["type": "stream"]).result
        #expect(out == ["content": [["type": "text", "text": "done"]]], "\(out.serialize())")
        var kinds: [String] = []
        var prog: JSON = .null
        while let item = inbox.tryTake() {
            let t = item.payload["type"].asString ?? ""
            if t == "progress" { prog = item.payload }
            kinds.append(t)
        }
        #expect(kinds.contains("progress") && kinds.contains("log"), "\(kinds)")
        #expect(prog["tool"] == "stream" && prog["total"] == 2)
    }

    @Test func inputRequiredAnswersWhatTheServerAsked() async throws {
        let stub = fakeMcp()
        let (k, dir) = await mk("http://127.0.0.1:\(stub.port)/mcp")

        defer { try? FileManager.default.removeItem(at: dir) }
        let out = await send(k, "mcp", ["type": "ask"])
        #expect(out.status == .errored)
        #expect(out.result["input_required"]["requestState"] == "st-1")
        #expect(out.result["input_required"]["inputRequests"]["who"] != .null)
    }

    @Test func failuresAreRefusalsFromTheClosedSet() async throws {
        let stub = fakeMcp()
        let (k, dir) = await mk("http://127.0.0.1:\(stub.port)/mcp")

        defer { try? FileManager.default.removeItem(at: dir) }
        for (payload, reason): (OrderedDictionary<String, JSON>, String) in [
            (["type": "weird"], "transport_error"),
            (["type": "cut"], "transport_closed"),
            (["type": "get_weather"], "bad_arg"),
            (["type": "no_such_tool"], "unhandled"),
        ] {
            let out = await send(k, "mcp", payload).result
            #expect(out["reason"] == .string(reason), "\(payload): \(out.serialize())")
        }
        let hint = await send(k, "mcp", ["type": "nope"]).result["hint"].asString ?? ""
        #expect(hint.contains("get_weather") && hint.contains("docs of=mcp"), "\(hint)")
    }

    @Test func aServerOnAnOlderProtocolIsRefusedNamingItsVersions() async throws {
        let stub = fakeMcp(versions: ["2025-11-25"])
        let (k, dir) = await mk("http://127.0.0.1:\(stub.port)/mcp")

        defer { try? FileManager.default.removeItem(at: dir) }
        let out = await send(k, "mcp", ["type": "boot"]).result
        #expect(out["reason"] == "transport_error")
        let e = out["error"].asString ?? ""
        #expect(e.contains("2025-11-25") && e.contains("2026-07-28 only"), "\(e)")
    }

    @Test func anUnsupportedVersionErrorNamesTheServersVersions() async throws {
        let stub = StubHTTP(respond: { request in
            let body = parse(request).body
            return reply(
                400,
                .object([
                    "jsonrpc": "2.0", "id": body["id"],
                    "error": [
                        "code": -32022, "message": "Unsupported protocol version",
                        "data": ["supported": ["2025-06-18"], "requested": "2026-07-28"],
                    ],
                ]))
        })
        let (k, dir) = await mk("http://127.0.0.1:\(stub.port)/mcp")

        defer { try? FileManager.default.removeItem(at: dir) }
        let out = await send(k, "mcp", ["type": "boot"]).result
        #expect(
            out["reason"] == "transport_error"
                && out["error"].asString?.contains("2025-06-18") == true)
    }

    @Test func anUnreachableServerIsNotConnectedAndCallsSayIt() async throws {
        let stub = StubHTTP([])
        let port = stub.port
        stub.stop()
        try await Task.sleep(nanoseconds: 50_000_000)
        let (k, dir) = await mk("http://127.0.0.1:\(port)/mcp")

        defer { try? FileManager.default.removeItem(at: dir) }
        let boot = await send(k, "mcp", ["type": "boot"]).result
        #expect(boot["reason"] == "not_connected", "\(boot.serialize())")
        let call = await send(k, "mcp", ["type": "get_weather", "city": "x"]).result
        #expect(
            call["reason"] == "not_connected"
                && call["error"].asString?.contains("cannot connect") == true)
        #expect(await send(k, "mcp", ["type": "reflect"]).result["last_error"] != .null)
    }

    @Test func aFailedRelistKeepsTheToolsAlreadyListed() async throws {
        let stub = fakeMcp()
        let (k, dir) = await mk("http://127.0.0.1:\(stub.port)/mcp")

        defer { try? FileManager.default.removeItem(at: dir) }
        stub.stop()
        try await Task.sleep(nanoseconds: 50_000_000)
        #expect(await send(k, "mcp", ["type": "boot"]).result["reason"] == "not_connected")
        let d = await send(k, "kernel", ["type": "docs", "of": "mcp"]).result
        #expect(d["verbs"]["get_weather"] != .null, "the earlier listing stands")
    }

    @Test func aGrantNamesToolsLikeVerbs() async throws {
        let stub = fakeMcp()
        let (k, dir) = await mk("http://127.0.0.1:\(stub.port)/mcp")

        defer { try? FileManager.default.removeItem(at: dir) }
        k.register(
            Agent(id: AgentId("caller"), handlerModule: nil, parentId: AgentId("kernel_state")))
        k.update(AgentId("caller"), ["can_call": ["mcp": ["get_weather"]]])
        k.update(AgentId("mcp"), ["callable_by": ["caller": ["get_weather", "fail"]]])
        let (okOut, no) = await KernelTaskLocals.$sender.withValue(AgentId("caller")) {
            (
                await send(k, "mcp", ["type": "get_weather", "city": "Kyiv"]),
                await send(k, "mcp", ["type": "fail"])
            )
        }
        #expect(okOut.status == .completed, "\(okOut.result.serialize())")
        #expect(
            no.result["reason"] == "unauthorized" && no.result["side"] == "egress",
            "\(no.result.serialize())")
    }

    @Test func theRecordIsCheckedAtTheWrite() {
        let b = McpClientBundle()
        for (meta, fragment): (OrderedDictionary<String, JSON>, String) in [
            ([:], "required on the record and missing: url"),
            (["url": "mcp.test/mcp"], "absolute http(s) URL"),
            (["url": "https://mcp.test/mcp#frag"], "no fragment"),
            (
                ["url": "https://mcp.test/mcp", "credentials": ["store": "creds"]],
                "credentials must be"
            ),
        ] {
            let e = b.validateMeta(meta, record: [:]) ?? ""
            #expect(e.contains(fragment), "\(meta): \(e)")
        }
    }

    @Test func theCatalogPublishesTheRecordSchema() async throws {
        let stub = fakeMcp()
        let (k, dir) = await mk("http://127.0.0.1:\(stub.port)/mcp")

        defer { try? FileManager.default.removeItem(at: dir) }
        let cat = await send(k, "kernel", ["type": "reflect", "tree": "none", "bundles": "all"])
            .result
        let row = try #require(cat["bundles"].asArray?.first { $0["name"] == "mcp_client" })
        #expect(
            (row["schema"]["fields"].asArray ?? []).map { $0["name"] } == ["url", "credentials"])
    }

    @Test func aCallPastItsTimeoutSettlesTimedOut() async throws {
        let stub = StubHTTP(respond: { request in
            let r = parse(request)
            if r.body["method"] == "tools/call" { return StubHTTP.Reply(status: 200, delay: 3) }
            return fakeAnswer(r)
        })
        let (k, dir) = await mk("http://127.0.0.1:\(stub.port)/mcp")

        defer { try? FileManager.default.removeItem(at: dir) }
        let out = await send(k, "mcp", ["type": "get_weather", "city": "x"], timeout: 0.5)
        #expect(out.status == .timedOut, "\(out.result.serialize())")
    }
}

/// The discover/list half of the fake, for a stub whose calls behave otherwise.
private func fakeAnswer(_ r: Seen) -> StubHTTP.Reply {
    let body = r.body
    switch body["method"].asString ?? "" {
    case "server/discover":
        return ok(body, ["supportedVersions": ["2026-07-28"], "capabilities": ["tools": [:]]])
    default:
        return ok(body, ["tools": .array(Array(TOOLS.prefix(1)))])
    }
}
