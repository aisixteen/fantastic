// HTTP host wire tests — the four python routes, Starlette matching, and the
// web_rest leg's request/response texts.

import FantasticJSON
import FantasticKernel
import FantasticKernelStartup
import Foundation
import Testing

@testable import FantasticWeb

@Suite("HTTP server", .serialized)
struct HTTPServerTests {
    /// Boot `web` on its record port and hand back the live port.
    private func bootWeb(_ kernel: Kernel) async -> UInt16 {
        _ = await kernel.send([Item(AgentId("web"), .object(["type": .string("boot")]))])[0].result
        return kernel.httpPort()
    }

    private func stopWeb(_ kernel: Kernel) async {
        _ = await kernel.send([Item(AgentId("web"), .object(["type": .string("stop")]))])[0].result
    }

    /// Seed an opened `web_rest` child granted `core`, mount it, return its id.
    private func mountRest(_ kernel: Kernel, canCall: JSON = .object(["core": .string("*")]))
        async -> String
    {
        let rec = await kernel.send([
            Item(
                AgentId("kernel"),
                .object([
                    "type": .string("create_agent"),
                    "parent_id": .string("web"),
                    "handler_module": .string("web_rest.tools"),
                    "id": .string("rest"),
                    "ingress_rule": .string("allow_all_ingress"),
                    "can_call": canCall,
                ]))
        ])[0].result
        let restId = rec["id"].asString ?? "rest"
        var grants: JSON = .object([
            "$self": .string("*"), "$parent": .string("*"), "$children": .string("*"),
        ])
        grants[restId] = .string("*")
        _ = await kernel.send([
            Item(
                AgentId("core"),
                .object([
                    "type": .string("update_agent"), "id": .string("core"),
                    "callable_by": grants,
                ]))
        ])[0].result
        _ = await kernel.send([
            Item(
                AgentId("web"),
                .object(["type": .string("mount"), "child_id": .string(restId)]))
        ])[0].result
        return restId
    }

    /// One raw HTTP exchange, so the response HEAD is visible byte for byte.
    private func rawRequest(port: UInt16, _ requestLines: String, keepOpen: Bool = false) throws
        -> String
    {
        let sock = try RawHTTPClient(port: port)
        defer { if !keepOpen { sock.close() } }
        try sock.write(requestLines)
        return sock.readSome()
    }

    // MARK: - A11.3 route table

    /// A11.3 — `/` is python's `_index_page`: the reflect tree rendered into
    /// `templates/index.html`.
    @Test func indexRendersTheReflectTree() async throws {
        let kernel = try await startWebKernel()
        let port = await bootWeb(kernel)
        defer { Task { await stopWeb(kernel) } }
        let (data, response) = try await URLSession(configuration: .ephemeral)
            .data(from: URL(string: "http://127.0.0.1:\(port)/")!)
        let http = try #require(response as? HTTPURLResponse)
        #expect(http.statusCode == 200)
        #expect(
            (http.value(forHTTPHeaderField: "Content-Type") ?? "") == "text/html; charset=utf-8")
        let html = String(data: data, encoding: .utf8) ?? ""
        // The scaffold is python's template, verbatim.
        let tpl = try String(
            contentsOf: pythonSource("web/host/src/web/templates/index.html"), encoding: .utf8)
        // The bundled scaffold is python's file, byte for byte.
        #expect(WebServer.indexTemplate == tpl)
        let head = String(tpl.prefix(while: { $0 != "{" }))
        #expect(html.hasPrefix(head))
        // ...with python's markup for every node.
        #expect(html.contains("<li><span class=\"id\">core</span> <code>core</code>"))
        #expect(html.contains("<span class=\"hm\">(root)</span>"))
        #expect(html.contains("<span class=\"hm\">web.tools</span>"))
        // The swift-only index is gone.
        #expect(!html.contains("fantastic kernel"))
    }

    /// A11.3 (D11) — the swift-only extras are gone; the favicon routes are
    /// python's two.
    @Test func renderRouteAndAssetsAreAbsent() async throws {
        let kernel = try await startWebKernel()
        let port = await bootWeb(kernel)
        defer { Task { await stopWeb(kernel) } }
        let session = URLSession(configuration: .ephemeral)
        for path in ["/_fantastic/transport.js", "/_assets/three.module.js", "/core/"] {
            let (data, response) = try await session.data(
                from: URL(string: "http://127.0.0.1:\(port)\(path)")!)
            let http = try #require(response as? HTTPURLResponse)
            #expect(http.statusCode == 404, "\(path)")
            #expect(String(data: data, encoding: .utf8) == "{\"detail\":\"Not Found\"}", "\(path)")
        }
        for path in ["/favicon.png", "/_assets/favicon.png"] {
            let (data, response) = try await session.data(
                from: URL(string: "http://127.0.0.1:\(port)\(path)")!)
            let http = try #require(response as? HTTPURLResponse)
            #expect(http.statusCode == 200, "\(path)")
            #expect(http.value(forHTTPHeaderField: "Content-Type") == "image/png", "\(path)")
            let expected = try Data(
                contentsOf: pythonSource("web/host/src/web/favicon.png"))
            #expect(data == expected, "\(path)")
        }
    }

    // MARK: - A11.10 file proxy

    /// A11.10 — the proxy types its body with CPython's `mimetypes` table
    /// (`.pdf` → application/pdf, unknown → octet-stream, `text/*` + charset).
    @Test func fileProxyContentTypeIsPythonMimetypes() async throws {
        let kernel = try await startWebKernel()
        let rel = "fantastic-mime-\(UUID().uuidString)"
        let dir = URL(fileURLWithPath: FileManager.default.currentDirectoryPath)
            .appendingPathComponent(rel)
        try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: dir) }
        for name in ["a.pdf", "b.weirdext", "c.txt"] {
            try "body".write(
                to: dir.appendingPathComponent(name), atomically: true, encoding: .utf8)
        }
        _ = await kernel.send([
            Item(
                AgentId("core"),
                .object([
                    "type": .string("create_agent"), "parent_id": .string("core"),
                    "handler_module": .string("file_bridge.tools"), "id": .string("files"),
                    "root": .string(rel), "ingress_rule": .string("allow_all_ingress"),
                ]))
        ])[0].result
        let port = await bootWeb(kernel)
        defer { Task { await stopWeb(kernel) } }
        let session = URLSession(configuration: .ephemeral)
        let expected = [
            "a.pdf": "application/pdf",
            "b.weirdext": "application/octet-stream",
            "c.txt": "text/plain; charset=utf-8",
        ]
        for (name, mime) in expected {
            let (data, response) = try await session.data(
                from: URL(string: "http://127.0.0.1:\(port)/files/file/\(name)")!)
            let http = try #require(response as? HTTPURLResponse)
            #expect(http.statusCode == 200, "\(name)")
            #expect(http.value(forHTTPHeaderField: "Content-Type") == mime, "\(name)")
            #expect(String(data: data, encoding: .utf8) == "body", "\(name)")
        }
        // The pure table, without a live socket.
        #expect(Mimetypes.guessType("x.pdf") == "application/pdf")
        #expect(Mimetypes.guessType("x.csv") == "text/csv")
        #expect(Mimetypes.guessType("x.wasm") == "application/wasm")
        #expect(Mimetypes.guessType("x.ico") == "image/vnd.microsoft.icon")
        #expect(Mimetypes.guessType("x.tar.gz") == "application/x-tar")
        #expect(Mimetypes.guessType("x.tgz") == "application/x-tar")
        #expect(Mimetypes.guessType(".bashrc") == nil)
        #expect(Mimetypes.guessType("x.nope") == nil)
    }

    /// A11.31 — a REST-originated dispatch is a CHILD of the web boot task
    /// (python serves under `CURRENT_TASK.scope`); swift rooted every request,
    /// so the task row's `parent` was null.
    @Test func testIngressDispatchParent() async throws {
        let kernel = try await startWebKernel()
        kernel.bundles.register("parent_probe.tools", ParentProbe())
        let port = await bootWeb(kernel)
        let restId = await mountRest(kernel, canCall: .object(["probe": .string("*")]))
        _ = await kernel.send([
            Item(
                AgentId("core"),
                .object([
                    "type": .string("create_agent"), "parent_id": .string("core"),
                    "handler_module": .string("parent_probe.tools"), "id": .string("probe"),
                    "callable_by": .object([
                        "$self": .string("*"), "$parent": .string("*"),
                        "$children": .string("*"), restId: .string("*"),
                    ]),
                ]))
        ])[0].result
        defer { Task { await stopWeb(kernel) } }
        var req = URLRequest(url: URL(string: "http://127.0.0.1:\(port)/\(restId)/probe")!)
        req.httpMethod = "POST"
        req.setValue("application/json", forHTTPHeaderField: "Content-Type")
        req.httpBody = Data("{\"type\":\"whoami\"}".utf8)
        let (data, _) = try await URLSession(configuration: .ephemeral).data(for: req)
        let reply = try JSON.parse(data)
        #expect(!reply["parent"].isNull, "REST dispatch is rooted, not parented: \(reply)")

        // An in-process call from the operator IS a root, so the assertion
        // above is about the request path, not about every dispatch.
        let direct = await kernel.send([
            Item(AgentId("probe"), .object(["type": .string("whoami")]))
        ])[0].result
        #expect(direct["parent"].isNull, "\(direct)")
    }

    // MARK: - A13.6 / A13.9 matching

    /// A13.6 — an unmatched path is FastAPI's 404 JSON; a path that matches
    /// under another method is a 405 with `allow`.
    @Test func unmatchedPathsAndMethodsAnswerFastAPIJSON() async throws {
        let kernel = try await startWebKernel()
        let port = await bootWeb(kernel)
        let restId = await mountRest(kernel)
        defer { Task { await stopWeb(kernel) } }
        // No route at all.
        let miss = try rawRequest(port: port, "GET /nope HTTP/1.1\r\nHost: h\r\n\r\n")
        #expect(miss.contains("HTTP/1.1 404 Not Found"), "\(miss)")
        #expect(miss.hasSuffix("{\"detail\":\"Not Found\"}"), "\(miss)")
        #expect(miss.contains("content-length: 22"), "\(miss)")
        // Path matches POST /<rest>/{target_id} only.
        for method in ["GET", "PUT", "HEAD"] {
            let r = try rawRequest(
                port: port, "\(method) /\(restId)/core HTTP/1.1\r\nHost: h\r\n\r\n")
            #expect(r.contains("HTTP/1.1 405 Method Not Allowed"), "\(method) → \(r)")
            #expect(r.contains("allow: POST"), "\(method) → \(r)")
        }
        // HEAD on a GET route: no implicit HEAD on a FastAPI APIRoute.
        let head = try rawRequest(
            port: port, "HEAD /\(restId)/_reflect/core HTTP/1.1\r\nHost: h\r\n\r\n")
        #expect(head.contains("HTTP/1.1 405 Method Not Allowed"), "\(head)")
        #expect(head.contains("allow: GET"), "\(head)")
    }

    /// A13.9 — `POST /<rest>/_reflect` is an ordinary `{target_id}` match, so it
    /// dispatches to an agent literally named `_reflect`.
    @Test func postToReflectLiteralDispatches() async throws {
        let kernel = try await startWebKernel()
        let port = await bootWeb(kernel)
        let restId = await mountRest(
            kernel, canCall: .object(["core": .string("*"), "_reflect": .string("*")]))
        defer { Task { await stopWeb(kernel) } }
        var req = URLRequest(url: URL(string: "http://127.0.0.1:\(port)/\(restId)/_reflect")!)
        req.httpMethod = "POST"
        req.httpBody = Data("{\"type\":\"reflect\"}".utf8)
        let (data, response) = try await URLSession(configuration: .ephemeral).data(for: req)
        let http = try #require(response as? HTTPURLResponse)
        #expect(http.statusCode == 200)
        let json = try JSON.parse(data)
        #expect(json["reason"].asString == "unknown_agent", "\(json)")
        #expect(json["error"].asString?.contains("_reflect") == true, "\(json)")
    }

    /// A13.9 — a path that only matches with the trailing slash toggled is a
    /// 307 carrying the absolute location and no body.
    @Test func trailingSlashAnswers307() async throws {
        let kernel = try await startWebKernel()
        let port = await bootWeb(kernel)
        let restId = await mountRest(kernel)
        defer { Task { await stopWeb(kernel) } }
        let r = try rawRequest(
            port: port, "POST /\(restId)/core/ HTTP/1.1\r\nHost: h\r\ncontent-length: 0\r\n\r\n")
        #expect(r.hasPrefix("HTTP/1.1 307 "), "\(r)")
        #expect(r.contains("location: http://h/\(restId)/core"), "\(r)")
        // starlette's RedirectResponse carries an empty body and no media type.
        #expect(r.contains("content-length: 0"), "\(r)")
        #expect(!r.contains("content-type"), "\(r)")
        let q = try rawRequest(
            port: port, "GET /\(restId)/_reflect/?tree=none HTTP/1.1\r\nHost: h\r\n\r\n")
        #expect(q.hasPrefix("HTTP/1.1 307 "), "\(q)")
        #expect(q.contains("location: http://h/\(restId)/_reflect?tree=none"), "\(q)")
    }

    // MARK: - A13.8 framing

    /// A13.8 — lowercase header names in python's order, `date` always, no
    /// `Connection: close`, and the channel serves a second request.
    @Test func restResponseHeadersMatchPython() async throws {
        let kernel = try await startWebKernel()
        let port = await bootWeb(kernel)
        defer { Task { await stopWeb(kernel) } }
        let raw = try rawRequest(port: port, "GET /nope HTTP/1.1\r\nHost: h\r\n\r\n")
        let headBlock = String(raw.components(separatedBy: "\r\n\r\n")[0])
        let names = headBlock.components(separatedBy: "\r\n").dropFirst().map {
            String($0.split(separator: ":")[0])
        }
        #expect(names == ["date", "server", "content-length", "content-type"], "\(names)")
        #expect(headBlock.contains("server: uvicorn"), "\(headBlock)")
        #expect(headBlock.contains("date: "), "\(headBlock)")
        #expect(!headBlock.lowercased().contains("connection: close"), "\(headBlock)")
    }

    /// A13.8 — the connection is KEPT: two requests on one socket.
    @Test func httpConnectionIsKeptAlive() async throws {
        let kernel = try await startWebKernel()
        let port = await bootWeb(kernel)
        defer { Task { await stopWeb(kernel) } }
        let sock = try RawHTTPClient(port: port)
        defer { sock.close() }
        try sock.write("GET /nope HTTP/1.1\r\nHost: h\r\n\r\n")
        let first = sock.readSome()
        #expect(first.contains("404"), "\(first)")
        try sock.write("GET /nope HTTP/1.1\r\nHost: h\r\n\r\n")
        let second = sock.readSome()
        #expect(second.contains("404"), "second request on the same socket: \(second)")
    }

    // MARK: - A11.27 / A13.2 / A13.4 / A13.5 the REST leg

    /// A11.27 — python's REST body texts, and an empty body is `{}`.
    @Test func webRestJSONBodyTexts() async throws {
        let kernel = try await startWebKernel()
        let port = await bootWeb(kernel)
        let restId = await mountRest(kernel)
        defer { Task { await stopWeb(kernel) } }
        let session = URLSession(configuration: .ephemeral)
        func post(_ body: String) async throws -> (JSON, Int) {
            var req = URLRequest(url: URL(string: "http://127.0.0.1:\(port)/\(restId)/core")!)
            req.httpMethod = "POST"
            req.httpBody = Data(body.utf8)
            let (data, response) = try await session.data(for: req)
            let http = try #require(response as? HTTPURLResponse)
            return (try JSON.parse(data), http.statusCode)
        }
        let (bad, badStatus) = try await post("not json")
        #expect(badStatus == 400)
        #expect(bad["error"].asString == "web_rest: request body is not valid JSON", "\(bad)")
        let (arr, arrStatus) = try await post("[1]")
        #expect(arrStatus == 400)
        #expect(arr["error"].asString == "web_rest: body must be a JSON object", "\(arr)")
        // An EMPTY body parses as `{}` — and an empty payload names no verb.
        let (empty, emptyStatus) = try await post("")
        #expect(emptyStatus == 200)
        #expect(empty["reason"].asString == "missing_arg", "\(empty)")
        #expect(empty["error"].asString?.contains("type") == true, "\(empty)")
    }

    /// A11.27 — the GET `_reflect` shortcuts read python's query flags.
    @Test func webRestReflectQueryFlags() async throws {
        let kernel = try await startWebKernel()
        let port = await bootWeb(kernel)
        let restId = await mountRest(kernel, canCall: .object(["core": .string("*")]))
        defer { Task { await stopWeb(kernel) } }
        let session = URLSession(configuration: .ephemeral)
        let (plain, _) = try await session.data(
            from: URL(string: "http://127.0.0.1:\(port)/\(restId)/_reflect")!)
        #expect(!(try JSON.parse(plain))["tree"].isNull)
        let (readme, _) = try await session.data(
            from: URL(string: "http://127.0.0.1:\(port)/\(restId)/_reflect?readme=1")!)
        #expect(!(try JSON.parse(readme))["readme"].isNull)
        let (noTree, _) = try await session.data(
            from: URL(string: "http://127.0.0.1:\(port)/\(restId)/_reflect?tree=none")!)
        #expect((try JSON.parse(noTree))["tree"].isNull)
        // `?tree=bogus` is not one of the three tiers — python drops it.
        let (bogus, _) = try await session.data(
            from: URL(string: "http://127.0.0.1:\(port)/\(restId)/_reflect?tree=bogus")!)
        #expect(!(try JSON.parse(bogus))["tree"].isNull)
    }

    /// A11.27 / A13.4 — the octet leg's header texts, and an EMPTY
    /// `X-Fantastic-Payload` is an empty envelope, not a parse error.
    @Test func emptyPayloadHeaderIsAnEmptyEnvelope() async throws {
        let kernel = try await startWebKernel()
        let port = await bootWeb(kernel)
        let restId = await mountRest(kernel)
        defer { Task { await stopWeb(kernel) } }
        let session = URLSession(configuration: .ephemeral)
        func octet(_ header: String?) async throws -> (JSON, Int) {
            var req = URLRequest(url: URL(string: "http://127.0.0.1:\(port)/\(restId)/core")!)
            req.httpMethod = "POST"
            req.setValue("application/octet-stream", forHTTPHeaderField: "Content-Type")
            if let header = header {
                req.setValue(header, forHTTPHeaderField: "X-Fantastic-Payload")
            }
            req.httpBody = Data("x".utf8)
            let (data, response) = try await session.data(for: req)
            let http = try #require(response as? HTTPURLResponse)
            return (try JSON.parse(data), http.statusCode)
        }
        // An empty header value is `{}` — it REACHES dispatch instead of
        // answering the 400 swift used to send.
        let (empty, emptyStatus) = try await octet("")
        #expect(emptyStatus == 200, "\(empty)")
        #expect(
            empty["error"].asString?.contains("X-Fantastic-Payload") != true, "\(empty)")
        let (bad, badStatus) = try await octet("nope")
        #expect(badStatus == 400)
        #expect(
            bad["error"].asString == "web_rest: X-Fantastic-Payload is not valid JSON", "\(bad)")
        let (arr, arrStatus) = try await octet("[1]")
        #expect(arrStatus == 400)
        #expect(
            arr["error"].asString == "web_rest: X-Fantastic-Payload must be a JSON object",
            "\(arr)")
    }

    /// A13.5 — a null reply is 204 with no body and no content-type (swift
    /// answered `jsonHTTP(200, null)`). Driven over the JSON leg: the octet
    /// branch takes the same decision, but `sendWithBinary` admits only the
    /// binary-edge verbs, so no null-answering verb reaches it until C37 moves
    /// that leg onto the batch.
    @Test func webRestOctetNullReplyIs204() async throws {
        let kernel = try await startWebKernel()
        let port = await bootWeb(kernel)
        let restId = await mountRest(kernel, canCall: .object(["$self": .string("*")]))
        defer { Task { await stopWeb(kernel) } }
        let raw = try rawRequest(
            port: port,
            "POST /\(restId)/\(restId) HTTP/1.1\r\nHost: h\r\n"
                + "content-type: application/json\r\n"
                + "content-length: \("{\"type\":\"boot\"}".utf8.count)\r\n\r\n"
                + "{\"type\":\"boot\"}")
        #expect(raw.contains("HTTP/1.1 204 No Content"), "\(raw)")
        #expect(!raw.contains("content-length"), "\(raw)")
        #expect(!raw.contains("content-type"), "\(raw)")
    }

    /// A13.2 / A11.20 (leg half) — the octet reply header is python's
    /// `json.dumps` (ensure_ascii, `, ` separators), and naming a field other
    /// than `bytes` sends NO blob, so a `write_stream` writes nothing.
    @Test func octetReplyPayloadHeaderIsPythonJSONDumps() async throws {
        let kernel = try await startWebKernel()
        let rel = "fantastic-octet-\(UUID().uuidString)"
        let dir = URL(fileURLWithPath: FileManager.default.currentDirectoryPath)
            .appendingPathComponent(rel)
        try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: dir) }
        // `naÃ¯ve.txt` is what the latin-1 header law (A13.12) makes of a raw
        // UTF-8 `naïve.txt` — so the reply carries non-ASCII the header must
        // escape.
        try "hi".write(
            to: dir.appendingPathComponent("naÃ¯ve.txt"), atomically: true, encoding: .utf8)
        _ = await kernel.send([
            Item(
                AgentId("core"),
                .object([
                    "type": .string("create_agent"), "parent_id": .string("core"),
                    "handler_module": .string("file_bridge.tools"), "id": .string("files"),
                    "root": .string(rel), "ingress_rule": .string("allow_all_ingress"),
                ]))
        ])[0].result
        let port = await bootWeb(kernel)
        let restId = await mountRest(kernel, canCall: .object(["files": .string("*")]))
        _ = await kernel.send([
            Item(
                AgentId("core"),
                .object([
                    "type": .string("update_agent"), "id": .string("files"),
                    "callable_by": .object([
                        "$self": .string("*"), "$parent": .string("*"),
                        "$children": .string("*"), restId: .string("*"),
                    ]),
                ]))
        ])[0].result
        defer { Task { await stopWeb(kernel) } }
        let sock = try RawHTTPClient(port: port)
        defer { sock.close() }
        try sock.write(
            "POST /\(restId)/files HTTP/1.1\r\nHost: h\r\n"
                + "content-type: application/octet-stream\r\n"
                + "x-fantastic-payload: {\"type\":\"read_stream\",\"path\":\"naïve.txt\"}\r\n"
                + "content-length: 0\r\n\r\n")
        let raw = sock.readSome()
        #expect(raw.contains("HTTP/1.1 200 OK"), "\(raw)")
        let header = try #require(
            raw.components(separatedBy: "\r\n").first {
                $0.hasPrefix("x-fantastic-payload: ")
            }, "\(raw)")
        // `json.dumps` escapes every non-ASCII scalar and spaces its
        // separators; `serialize()` sent compact raw UTF-8.
        #expect(header.contains("na\\u00c3\\u00afve.txt"), "\(header)")
        #expect(header.contains("\", \""), "\(header)")
        #expect(raw.contains("x-fantastic-bytes-field: bytes"), "\(raw)")
        #expect(raw.hasSuffix("hi"), "\(raw)")
    }

    /// A11.20 (leg half) — the body lands under the field
    /// `X-Fantastic-Bytes-Field` names. `bytes` (or absent) binds the blob;
    /// ANY other name leaves the header alone and sends no chunk at all.
    @Test func webRestOctetBytesFieldNamesThePayloadField() async throws {
        let kernel = try await startWebKernel()
        let rel = "fantastic-bfield-\(UUID().uuidString)"
        let dir = URL(fileURLWithPath: FileManager.default.currentDirectoryPath)
            .appendingPathComponent(rel)
        try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: dir) }
        _ = await kernel.send([
            Item(
                AgentId("core"),
                .object([
                    "type": .string("create_agent"), "parent_id": .string("core"),
                    "handler_module": .string("file_bridge.tools"), "id": .string("files"),
                    "root": .string(rel), "ingress_rule": .string("allow_all_ingress"),
                ]))
        ])[0].result
        let port = await bootWeb(kernel)
        let restId = await mountRest(kernel, canCall: .object(["files": .string("*")]))
        _ = await kernel.send([
            Item(
                AgentId("core"),
                .object([
                    "type": .string("update_agent"), "id": .string("files"),
                    "callable_by": .object([
                        "$self": .string("*"), "$parent": .string("*"),
                        "$children": .string("*"), restId: .string("*"),
                    ]),
                ]))
        ])[0].result
        defer { Task { await stopWeb(kernel) } }
        let session = URLSession(configuration: .ephemeral)
        func write(field: String?, path: String) async throws -> JSON {
            var req = URLRequest(url: URL(string: "http://127.0.0.1:\(port)/\(restId)/files")!)
            req.httpMethod = "POST"
            req.setValue("application/octet-stream", forHTTPHeaderField: "Content-Type")
            req.setValue(
                "{\"type\":\"write_stream\",\"path\":\"\(path)\",\"truncate\":true}",
                forHTTPHeaderField: "X-Fantastic-Payload")
            if let field = field {
                req.setValue(field, forHTTPHeaderField: "X-Fantastic-Bytes-Field")
            }
            req.httpBody = Data("abcdef".utf8)
            let (data, _) = try await session.data(for: req)
            return try JSON.parse(data)
        }
        let named = try await write(field: "bytes", path: "named.bin")
        #expect(named["written"].asInt == 6, "\(named)")
        let defaulted = try await write(field: nil, path: "default.bin")
        #expect(defaulted["written"].asInt == 6, "\(defaulted)")
        let other = try await write(field: "data", path: "custom.bin")
        #expect(other["written"].asInt == 0, "\(other)")
        #expect((try? Data(contentsOf: dir.appendingPathComponent("custom.bin")))?.count == 0)
    }

    /// A13.12 — request header values decode byte-for-byte as latin-1, so a raw
    /// UTF-8 path in `X-Fantastic-Payload` names a mojibake file.
    @Test func requestHeaderValuesDecodeAsLatin1() async throws {
        let kernel = try await startWebKernel()
        let rel = "fantastic-latin1-\(UUID().uuidString)"
        let dir = URL(fileURLWithPath: FileManager.default.currentDirectoryPath)
            .appendingPathComponent(rel)
        try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: dir) }
        try "hi".write(
            to: dir.appendingPathComponent("naïve.txt"), atomically: true, encoding: .utf8)
        _ = await kernel.send([
            Item(
                AgentId("core"),
                .object([
                    "type": .string("create_agent"), "parent_id": .string("core"),
                    "handler_module": .string("file_bridge.tools"), "id": .string("files"),
                    "root": .string(rel), "ingress_rule": .string("allow_all_ingress"),
                ]))
        ])[0].result
        let port = await bootWeb(kernel)
        let restId = await mountRest(kernel, canCall: .object(["files": .string("*")]))
        _ = await kernel.send([
            Item(
                AgentId("core"),
                .object([
                    "type": .string("update_agent"), "id": .string("files"),
                    "callable_by": .object([
                        "$self": .string("*"), "$parent": .string("*"),
                        "$children": .string("*"), restId: .string("*"),
                    ]),
                ]))
        ])[0].result
        defer { Task { await stopWeb(kernel) } }
        let raw = try rawRequest(
            port: port,
            "POST /\(restId)/files HTTP/1.1\r\nHost: h\r\n"
                + "content-type: application/octet-stream\r\n"
                + "x-fantastic-payload: {\"type\":\"read_stream\",\"path\":\"naïve.txt\"}\r\n"
                + "content-length: 0\r\n\r\n")
        // latin-1 turns the raw UTF-8 bytes into `naÃ¯ve.txt`, which is not there.
        #expect(raw.contains("not_found"), "\(raw)")
        #expect(raw.contains("naÃ¯ve.txt"), "\(raw)")
        // The pure decoder.
        #expect(latin1("naïve") == "naÃ¯ve")
    }
}

/// A minimal blocking HTTP/1.1 client — URLSession hides the response head,
/// and these tests pin header NAMES, ORDER and connection reuse.
final class RawHTTPClient {
    private let fd: Int32

    init(port: UInt16) throws {
        fd = socket(AF_INET, Int32(SOCK_STREAM), 0)
        guard fd >= 0 else { throw RawHTTPError.socket }
        var addr = sockaddr_in()
        addr.sin_family = sa_family_t(AF_INET)
        addr.sin_port = port.bigEndian
        addr.sin_addr.s_addr = inet_addr("127.0.0.1")
        let ok = withUnsafePointer(to: &addr) { p in
            p.withMemoryRebound(to: sockaddr.self, capacity: 1) {
                connect(fd, $0, socklen_t(MemoryLayout<sockaddr_in>.size))
            }
        }
        guard ok == 0 else { throw RawHTTPError.connect }
        var tv = timeval(tv_sec: 3, tv_usec: 0)
        setsockopt(fd, SOL_SOCKET, SO_RCVTIMEO, &tv, socklen_t(MemoryLayout<timeval>.size))
    }

    func write(_ s: String) throws { try writeBytes(Array(s.utf8)) }

    /// Write the string's scalars as LATIN-1 bytes (each scalar must be < 256) —
    /// the encoding python's ASGI layer reads header values in.
    func writeLatin1(_ s: String) throws {
        try writeBytes(s.unicodeScalars.map { UInt8($0.value & 0xFF) })
    }

    func writeBytes(_ bytes: [UInt8]) throws {
        var sent = 0
        while sent < bytes.count {
            let n = bytes.withUnsafeBytes {
                send(fd, $0.baseAddress!.advanced(by: sent), bytes.count - sent, 0)
            }
            guard n > 0 else { throw RawHTTPError.write }
            sent += n
        }
    }

    /// Read ONE response (head + declared body), leaving the socket open.
    func readSome() -> String {
        var out = Data()
        var buf = [UInt8](repeating: 0, count: 4096)
        while true {
            let n = recv(fd, &buf, buf.count, 0)
            if n <= 0 { break }
            out.append(contentsOf: buf[0..<n])
            let s = String(decoding: out, as: UTF8.self)
            guard let headEnd = s.range(of: "\r\n\r\n") else { continue }
            let head = String(s[..<headEnd.lowerBound])
            let declared =
                head.split(separator: "\r\n")
                .first { $0.lowercased().hasPrefix("content-length:") }
                .flatMap { Int($0.split(separator: ":")[1].trimmingCharacters(in: .whitespaces)) }
                ?? 0
            if s[headEnd.upperBound...].utf8.count >= declared { return s }
        }
        return String(decoding: out, as: UTF8.self)
    }

    func close() { _ = Foundation.close(fd) }
}

enum RawHTTPError: Error { case socket, connect, write }

/// Answers with the parent of the task row its own dispatch runs under — the
/// one fact A11.31 changes (python parents a request task on the web boot pid;
/// swift rooted it).
struct ParentProbe: AgentBundle {
    let name = "parent_probe"
    func handle(agentId: AgentId, payload: JSON, kernel: Kernel) async throws -> JSON? {
        let mine = KernelTaskLocals.currentTask
        let row = mine.flatMap { kernel.rows.getOne($0.value) }
        return .object([
            "task": mine.map { JSON.integer($0.value) } ?? .null,
            "parent": row?["parent"] ?? .null,
        ])
    }
}
