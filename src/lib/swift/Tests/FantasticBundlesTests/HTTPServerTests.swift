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
    private func mountRest(
        _ kernel: Kernel, canCall: JSON = .object(["kernel_state": .string("*")])
    )
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
                AgentId("kernel_state"),
                .object([
                    "type": .string("update_agent"), "id": .string("kernel_state"),
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

    /// The host renders nothing of its own: `/` is undeclared, so it is a 404 —
    /// no tree index, nothing about the kernel leaks to an anonymous client.
    @Test func rootIsNotAPage() async throws {
        let kernel = try await startWebKernel()
        let port = await bootWeb(kernel)
        defer { Task { await stopWeb(kernel) } }
        let (data, response) = try await URLSession(configuration: .ephemeral)
            .data(from: URL(string: "http://127.0.0.1:\(port)/")!)
        let http = try #require(response as? HTTPURLResponse)
        #expect(http.statusCode == 404)
        #expect(!(String(data: data, encoding: .utf8) ?? "").contains("kernel_state"))
    }

    /// A11.3 (D11) — the swift-only extras are gone; the favicon routes are
    /// python's two.
    @Test func renderRouteAndAssetsAreAbsent() async throws {
        let kernel = try await startWebKernel()
        let port = await bootWeb(kernel)
        defer { Task { await stopWeb(kernel) } }
        let session = URLSession(configuration: .ephemeral)
        for path in ["/_fantastic/transport.js", "/_assets/three.module.js", "/kernel_state/"] {
            let (data, response) = try await session.data(
                from: URL(string: "http://127.0.0.1:\(port)\(path)")!)
            let http = try #require(response as? HTTPURLResponse)
            #expect(http.statusCode == 404, "\(path)")
            #expect(String(data: data, encoding: .utf8) == "{\"detail\":\"Not Found\"}", "\(path)")
        }
        for path in ["/_assets/favicon.png"] {
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
                AgentId("kernel_state"),
                .object([
                    "type": .string("create_agent"), "parent_id": .string("kernel_state"),
                    "handler_module": .string("file_bridge.tools"), "allow": ["."], "id": .string("files"),
                    "root": .string(rel), "ingress_rule": .string("allow_all_ingress"),
                    // THE SERVING ALLOWANCE IS BOTH GATES (C41): the proxy
                    // dispatches AS `web`, so the served agent's `callable_by`
                    // decides whether the browser may reach it at all. Before
                    // that the route ran as the OPERATOR and `callable_by` was
                    // never consulted.
                    "callable_by": .object([
                        "$self": .string("*"), "$parent": .string("*"),
                        "$children": .string("*"), "web": .string("*"),
                    ]),
                ]))
        ])[0].result
        _ = await kernel.send([
            Item(
                AgentId("kernel"),
                .object([
                    "type": .string("update_agent"), "id": .string("web"),
                    "can_call": .object(["files": .string("*")]),
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

    /// python's `StreamingResponse`: a file over one 262144-byte chunk is
    /// STREAMED — every chunk its own lazy `read_stream`, the head declaring
    /// the first chunk's `size` — and arrives whole.
    @Test func aFileOverOneChunkStreamsWithTheDeclaredLength() async throws {
        let kernel = try await startWebKernel()
        let rel = "fantastic-stream-\(UUID().uuidString)"
        let dir = URL(fileURLWithPath: FileManager.default.currentDirectoryPath)
            .appendingPathComponent(rel)
        try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: dir) }
        let blob = Data((0..<600_000).map { UInt8($0 % 251) })
        try blob.write(to: dir.appendingPathComponent("big.bin"))
        _ = await kernel.send([
            Item(
                AgentId("kernel_state"),
                .object([
                    "type": .string("create_agent"), "parent_id": .string("kernel_state"),
                    "handler_module": .string("file_bridge.tools"), "allow": ["."],
                    "id": .string("files"), "root": .string(rel),
                    "ingress_rule": .string("allow_all_ingress"),
                    "callable_by": .object(["$parent": .string("*"), "web": .string("*")]),
                ]))
        ])[0].result
        _ = await kernel.send([
            Item(
                AgentId("kernel"),
                .object([
                    "type": .string("update_agent"), "id": .string("web"),
                    "can_call": .object(["files": .string("*")]),
                ]))
        ])[0].result
        let port = await bootWeb(kernel)
        defer { Task { await stopWeb(kernel) } }
        let (data, response) = try await URLSession(configuration: .ephemeral).data(
            from: URL(string: "http://127.0.0.1:\(port)/files/file/big.bin")!)
        let http = try #require(response as? HTTPURLResponse)
        #expect(http.statusCode == 200)
        #expect(http.value(forHTTPHeaderField: "Content-Length") == "600000")
        #expect(data == blob)
        let reads = kernel.rows.all().filter {
            $0["agent"]?.asString == "files" && $0["verb"]?.asString == "read_stream"
        }
        #expect(reads.count == 3, "one read per 262144-byte chunk: \(reads.count)")
    }

    /// A chunk that fails MID-FILE ends the body short of the length the head
    /// declared — a truncated transfer, never a shorter file that looks
    /// complete.
    @Test func aChunkThatFailsMidFileTruncatesTheTransfer() async throws {
        let kernel = try await startWebKernel()
        kernel.bundles.register("short_source.tools", ShortSource())
        _ = await kernel.send([
            Item(
                AgentId("kernel_state"),
                .object([
                    "type": .string("create_agent"), "parent_id": .string("kernel_state"),
                    "handler_module": .string("short_source.tools"), "id": .string("short"),
                    "callable_by": .object(["$parent": .string("*"), "web": .string("*")]),
                ]))
        ])[0].result
        _ = await kernel.send([
            Item(
                AgentId("kernel"),
                .object([
                    "type": .string("update_agent"), "id": .string("web"),
                    "can_call": .object(["short": .string("*")]),
                ]))
        ])[0].result
        let port = await bootWeb(kernel)
        defer { Task { await stopWeb(kernel) } }
        do {
            let (data, response) = try await URLSession(configuration: .ephemeral).data(
                from: URL(string: "http://127.0.0.1:\(port)/short/file/x.bin")!)
            let http = try #require(response as? HTTPURLResponse)
            #expect(http.value(forHTTPHeaderField: "Content-Length") == "10")
            #expect(data.count < 10, "a short body read as a complete file")
        } catch {
            // The client saw the connection end mid-body — the truncated
            // transfer itself.
        }
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
                AgentId("kernel_state"),
                .object([
                    "type": .string("create_agent"), "parent_id": .string("kernel_state"),
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
        // NO parent (python clears `_current_task` for a REST task, 2026-09-20):
        // the boot dispatch that used to hand its pid down has long settled, so
        // the table held a DANGLING parent.
        #expect(reply["parent"].isNull, "REST dispatch is parented: \(reply)")
        #expect(reply["ps_parent"].isNull, "ps holds a parent for the request: \(reply)")

        // An in-process call from the operator IS a root, so the assertion
        // above is about the request path, not about every dispatch.
        let direct = await kernel.send([
            Item(AgentId("probe"), .object(["type": .string("whoami")]))
        ])[0].result
        #expect(direct["parent"].isNull, "\(direct)")
        #expect(direct["ps_parent"].isNull, "\(direct)")
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
                port: port, "\(method) /\(restId)/kernel_state HTTP/1.1\r\nHost: h\r\n\r\n")
            #expect(r.contains("HTTP/1.1 405 Method Not Allowed"), "\(method) → \(r)")
            #expect(r.contains("allow: POST"), "\(method) → \(r)")
        }
        // HEAD on a GET route: no implicit HEAD on a FastAPI APIRoute.
        let head = try rawRequest(
            port: port, "HEAD /\(restId)/_reflect/kernel_state HTTP/1.1\r\nHost: h\r\n\r\n")
        #expect(head.contains("HTTP/1.1 405 Method Not Allowed"), "\(head)")
        #expect(head.contains("allow: GET"), "\(head)")
    }

    /// A13.9 — `POST /<rest>/_reflect` is an ordinary `{target_id}` match, so it
    /// dispatches to an agent literally named `_reflect`.
    @Test func postToReflectLiteralDispatches() async throws {
        let kernel = try await startWebKernel()
        let port = await bootWeb(kernel)
        let restId = await mountRest(
            kernel, canCall: .object(["kernel_state": .string("*"), "_reflect": .string("*")]))
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
            port: port,
            "POST /\(restId)/kernel_state/ HTTP/1.1\r\nHost: h\r\ncontent-length: 0\r\n\r\n")
        #expect(r.hasPrefix("HTTP/1.1 307 "), "\(r)")
        #expect(r.contains("location: http://h/\(restId)/kernel_state"), "\(r)")
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

    /// h11 appends `connection: close` — LAST, after the framing headers —
    /// whenever the cycle will not be kept alive: the request carried a `close`
    /// token, or spoke HTTP/1.0 (h11 has no 1.0 keep-alive).
    @Test func connectionCloseIsEchoedLikeH11() async throws {
        let kernel = try await startWebKernel()
        let port = await bootWeb(kernel)
        defer { Task { await stopWeb(kernel) } }
        for request in [
            "GET /nope HTTP/1.1\r\nHost: h\r\nConnection: close\r\n\r\n",
            "GET /nope HTTP/1.0\r\nHost: h\r\n\r\n",
        ] {
            let raw = try rawRequest(port: port, request)
            let headBlock = String(raw.components(separatedBy: "\r\n\r\n")[0])
            let names = headBlock.components(separatedBy: "\r\n").dropFirst().map {
                String($0.split(separator: ":")[0])
            }
            #expect(
                names == ["date", "server", "content-length", "content-type", "connection"],
                "\(request) → \(names)")
            #expect(headBlock.contains("connection: close"), "\(headBlock)")
        }
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
            var req = URLRequest(
                url: URL(string: "http://127.0.0.1:\(port)/\(restId)/kernel_state")!)
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
        let restId = await mountRest(kernel, canCall: .object(["kernel_state": .string("*")]))
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
            var req = URLRequest(
                url: URL(string: "http://127.0.0.1:\(port)/\(restId)/kernel_state")!)
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

    /// A `file_bridge` under the running dir, opened inbound and callable by the
    /// REST leg + `web`. Returns (restId, dir).
    private func mountFilesAndRest(_ kernel: Kernel, prefix: String) async throws -> (
        String, URL
    ) {
        let rel = "\(prefix)-\(UUID().uuidString)"
        let dir = URL(fileURLWithPath: FileManager.default.currentDirectoryPath)
            .appendingPathComponent(rel)
        try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        _ = await kernel.send([
            Item(
                AgentId("kernel_state"),
                .object([
                    "type": .string("create_agent"), "parent_id": .string("kernel_state"),
                    "handler_module": .string("file_bridge.tools"), "allow": ["."], "id": .string("files"),
                    "root": .string(rel), "ingress_rule": .string("allow_all_ingress"),
                ]))
        ])[0].result
        let restId = await mountRest(kernel, canCall: .object(["files": .string("*")]))
        _ = await kernel.send([
            Item(
                AgentId("kernel_state"),
                .object([
                    "type": .string("update_agent"), "id": .string("files"),
                    "callable_by": .object([
                        "$self": .string("*"), "$parent": .string("*"),
                        "$children": .string("*"), restId: .string("*"), "web": .string("*"),
                    ]),
                ]))
        ])[0].result
        return (restId, dir)
    }

    /// A13.1 — A REPLY THAT HOLDS A CHUNK IS OCTET ON BOTH LEGS, a 0-byte one
    /// included. Python: JSON POST `{type:read_stream}` → 200
    /// `application/octet-stream`, body = the chunk, `X-Fantastic-Payload` = the
    /// reply minus the field. Swift answered `application/json` with `bytes` as
    /// a JSON string and gated octet on a NON-EMPTY body, so an empty file came
    /// back as JSON.
    @Test func webRestJsonLegReadStreamAnswersOctet() async throws {
        let kernel = try await startWebKernel()
        let port = await bootWeb(kernel)
        let (restId, dir) = try await mountFilesAndRest(kernel, prefix: "fantastic-jsonoctet")
        defer { try? FileManager.default.removeItem(at: dir) }
        defer { Task { await stopWeb(kernel) } }
        try "hi".write(
            to: dir.appendingPathComponent("a.txt"), atomically: true, encoding: .utf8)
        FileManager.default.createFile(
            atPath: dir.appendingPathComponent("empty.bin").path, contents: Data())
        let session = URLSession(configuration: .ephemeral)
        func read(_ path: String) async throws -> (Data, HTTPURLResponse) {
            var req = URLRequest(url: URL(string: "http://127.0.0.1:\(port)/\(restId)/files")!)
            req.httpMethod = "POST"
            req.setValue("application/json", forHTTPHeaderField: "Content-Type")
            req.httpBody = Data("{\"type\":\"read_stream\",\"path\":\"\(path)\"}".utf8)
            let (d, r) = try await session.data(for: req)
            return (d, r as! HTTPURLResponse)
        }
        let (body, resp) = try await read("a.txt")
        #expect(resp.statusCode == 200)
        #expect(
            resp.value(forHTTPHeaderField: "Content-Type") == "application/octet-stream",
            "\(resp.allHeaderFields)")
        #expect(body == Data("hi".utf8))
        #expect(resp.value(forHTTPHeaderField: "X-Fantastic-Bytes-Field") == "bytes")
        let head = try #require(resp.value(forHTTPHeaderField: "X-Fantastic-Payload"))
        #expect(!head.contains("\"bytes\""), "the chunk must not ride the header: \(head)")
        // AN EMPTY FILE IS STILL OCTET — content-length 0, not a JSON body.
        let (empty, eresp) = try await read("empty.bin")
        #expect(
            eresp.value(forHTTPHeaderField: "Content-Type") == "application/octet-stream",
            "an empty chunk fell back to JSON: \(eresp.allHeaderFields)")
        #expect(empty.isEmpty)
        // A REFUSAL STAYS JSON.
        let (miss, mresp) = try await read("nope.txt")
        #expect(mresp.value(forHTTPHeaderField: "Content-Type") == "application/json")
        #expect(try JSON.parse(miss)["reason"].asString == "not_found")
    }

    /// A13.1 — on the JSON leg a `write_stream` carrying a JSON-STRING `bytes`
    /// is `missing_arg` and nothing is written: the request blob is empty and
    /// the header's own `bytes` key is what file_bridge refuses (A13.19).
    @Test func jsonLegWriteStreamStringBytesIsMissingArg() async throws {
        let kernel = try await startWebKernel()
        let port = await bootWeb(kernel)
        let (restId, dir) = try await mountFilesAndRest(kernel, prefix: "fantastic-jsonwrite")
        defer { try? FileManager.default.removeItem(at: dir) }
        defer { Task { await stopWeb(kernel) } }
        var req = URLRequest(url: URL(string: "http://127.0.0.1:\(port)/\(restId)/files")!)
        req.httpMethod = "POST"
        req.setValue("application/json", forHTTPHeaderField: "Content-Type")
        req.httpBody = Data(
            "{\"type\":\"write_stream\",\"path\":\"w.txt\",\"bytes\":\"abc\"}".utf8)
        let (d, _) = try await URLSession(configuration: .ephemeral).data(for: req)
        let reply = try JSON.parse(d)
        #expect(reply["reason"].asString == "missing_arg", "\(reply)")
        #expect(
            reply["error"].asString == "write_stream: bytes (one raw chunk) required", "\(reply)")
        #expect(
            !FileManager.default.fileExists(atPath: dir.appendingPathComponent("w.txt").path),
            "the refused write still wrote")
    }

    /// A13.3 — THE OCTET LEG DISPATCHES THROUGH THE BATCH, so it NAMES its
    /// execution: a repeated `X-Fantastic-Execution-Id` on the same target
    /// REPLAYS the recorded outcome instead of re-running the write, and a
    /// missing `payload.type` gets the KERNEL's refusal, not the bundle's.
    @Test func webRestOctetLegUsesTheBatch() async throws {
        let kernel = try await startWebKernel()
        let port = await bootWeb(kernel)
        let (restId, dir) = try await mountFilesAndRest(kernel, prefix: "fantastic-octetbatch")
        defer { try? FileManager.default.removeItem(at: dir) }
        defer { Task { await stopWeb(kernel) } }
        let session = URLSession(configuration: .ephemeral)
        func post(_ payload: String?, execId: String?, body: String) async throws -> JSON {
            var req = URLRequest(url: URL(string: "http://127.0.0.1:\(port)/\(restId)/files")!)
            req.httpMethod = "POST"
            req.setValue("application/octet-stream", forHTTPHeaderField: "Content-Type")
            if let payload { req.setValue(payload, forHTTPHeaderField: "X-Fantastic-Payload") }
            if let execId {
                req.setValue(execId, forHTTPHeaderField: "X-Fantastic-Execution-Id")
            }
            req.httpBody = Data(body.utf8)
            let (d, _) = try await session.data(for: req)
            return (try? JSON.parse(d)) ?? .null
        }
        // No payload header at all: the KERNEL refuses `send: payload.type
        // required`; swift used to reach the bundle with `unknown type ""`.
        let bare = try await post(nil, execId: nil, body: "x")
        #expect(bare["error"].asString == "send: payload.type required", "\(bare)")
        // A NAMED execution replays instead of re-running.
        let hdr = "{\"type\":\"write_stream\",\"path\":\"oe.bin\",\"truncate\":true}"
        let one = try await post(hdr, execId: "oe1", body: "one")
        #expect(one["written"].asInt == 3, "\(one)")
        let two = try await post(hdr, execId: "oe1", body: "twotwo")
        #expect(two["written"].asInt == 3, "a replayed exec id re-ran the write: \(two)")
        let onDisk = try Data(contentsOf: dir.appendingPathComponent("oe.bin"))
        #expect(String(decoding: onDisk, as: UTF8.self) == "one", "\(onDisk.count)")
    }

    /// A13.10 — the path is percent-decoded ONCE and its query stripped before
    /// matching AND before the file proxy. Swift matched the RAW target:
    /// `POST /rest/fi%6Ces` answered `no agent "fi%6Ces"`, and
    /// `/files/file/hello.txt?v=1` looked for a file named `hello.txt?v=1`.
    @Test func percentEncodedPathAndQueryResolveLikePython() async throws {
        let kernel = try await startWebKernel()
        let port = await bootWeb(kernel)
        let (restId, dir) = try await mountFilesAndRest(kernel, prefix: "fantastic-pct")
        defer { try? FileManager.default.removeItem(at: dir) }
        defer { Task { await stopWeb(kernel) } }
        try "hello".write(
            to: dir.appendingPathComponent("hello.txt"), atomically: true, encoding: .utf8)
        try "spaced".write(
            to: dir.appendingPathComponent("sp ace.txt"), atomically: true, encoding: .utf8)
        _ = await kernel.send([
            Item(
                AgentId("kernel"),
                .object([
                    "type": .string("update_agent"), "id": .string("web"),
                    "can_call": .object(["files": .string("*")]),
                ]))
        ])[0].result
        let session = URLSession(configuration: .ephemeral)
        // A percent-encoded AGENT ID in the REST route.
        var req = URLRequest(
            url: URL(string: "http://127.0.0.1:\(port)/\(restId)/fi%6Ces")!)
        req.httpMethod = "POST"
        req.setValue("application/json", forHTTPHeaderField: "Content-Type")
        req.httpBody = Data("{\"type\":\"boot\"}".utf8)
        let (_, resp) = try await session.data(for: req)
        #expect((resp as? HTTPURLResponse)?.statusCode == 204, "\(resp)")
        // A percent-encoded FILE NAME on the proxy…
        let (spaced, sresp) = try await session.data(
            from: URL(string: "http://127.0.0.1:\(port)/files/file/sp%20ace.txt")!)
        #expect((sresp as? HTTPURLResponse)?.statusCode == 200, "\(sresp)")
        #expect(String(decoding: spaced, as: UTF8.self) == "spaced")
        // …and a QUERY STRING is not part of the name.
        let (hello, hresp) = try await session.data(
            from: URL(string: "http://127.0.0.1:\(port)/files/file/hello.txt?v=1")!)
        #expect((hresp as? HTTPURLResponse)?.statusCode == 200, "\(hresp)")
        #expect(String(decoding: hello, as: UTF8.self) == "hello")
    }

    /// A13.11 (D13) — the JSON body decodes like CPython `json.loads(bytes)`:
    /// the encoding comes from the BOM or from the null-byte pattern of the
    /// first four bytes, so a UTF-8-BOM / UTF-16 / UTF-32 body is ordinary JSON.
    /// Bytes that will not decode are web_rest's own 400 — python catches the
    /// `UnicodeDecodeError` with the `JSONDecodeError` (it answered starlette's
    /// 500 text/plain until the leg caught it; parity 2026-10-01).
    @Test func jsonBodyDecodesLikeCPythonJsonLoads() async throws {
        let kernel = try await startWebKernel()
        let port = await bootWeb(kernel)
        let restId = await mountRest(kernel, canCall: .object(["$self": .string("*")]))
        defer { Task { await stopWeb(kernel) } }
        let session = URLSession(configuration: .ephemeral)
        func post(_ body: Data) async throws -> HTTPURLResponse {
            var req = URLRequest(
                url: URL(string: "http://127.0.0.1:\(port)/\(restId)/\(restId)")!)
            req.httpMethod = "POST"
            req.setValue("application/json", forHTTPHeaderField: "Content-Type")
            req.httpBody = body
            let (_, r) = try await session.data(for: req)
            return r as! HTTPURLResponse
        }
        let doc = "{\"type\":\"boot\"}"
        // UTF-8 with a BOM.
        var bom = Data([0xEF, 0xBB, 0xBF])
        bom.append(Data(doc.utf8))
        #expect(try await post(bom).statusCode == 204, "utf-8-sig")
        // UTF-16 LE with a BOM.
        let u16 = doc.data(using: .utf16LittleEndian)!
        var u16bom = Data([0xFF, 0xFE])
        u16bom.append(u16)
        #expect(try await post(u16bom).statusCode == 204, "utf-16 BOM")
        // UTF-16 BE with NO BOM — detected from `00 XX`.
        #expect(
            try await post(doc.data(using: .utf16BigEndian)!).statusCode == 204, "utf-16-be")
        // UTF-32 LE with NO BOM — `XX 00 00 00`.
        #expect(
            try await post(doc.data(using: .utf32LittleEndian)!).statusCode == 204, "utf-32-le")
        // Bytes that decode as nothing: `ff fe` names UTF-16-LE and one odd
        // trailing byte is a decode error → the leg's 400, as for bad JSON.
        let bad = try await post(Data([0xFF, 0xFE, 0x7B]))
        #expect(bad.statusCode == 400, "\(bad)")
        #expect(
            bad.value(forHTTPHeaderField: "Content-Type")?.hasPrefix("application/json") == true,
            "\(bad.allHeaderFields)")
        // The pure table, without a socket.
        #expect(WebServer.decodeJSONBody(Data()) == "")
        #expect(WebServer.decodeJSONBody(Data([0xFF, 0xFE, 0x7B])) == nil)
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
                AgentId("kernel_state"),
                .object([
                    "type": .string("create_agent"), "parent_id": .string("kernel_state"),
                    "handler_module": .string("file_bridge.tools"), "allow": ["."], "id": .string("files"),
                    "root": .string(rel), "ingress_rule": .string("allow_all_ingress"),
                ]))
        ])[0].result
        let port = await bootWeb(kernel)
        let restId = await mountRest(kernel, canCall: .object(["files": .string("*")]))
        _ = await kernel.send([
            Item(
                AgentId("kernel_state"),
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
                AgentId("kernel_state"),
                .object([
                    "type": .string("create_agent"), "parent_id": .string("kernel_state"),
                    "handler_module": .string("file_bridge.tools"), "allow": ["."], "id": .string("files"),
                    "root": .string(rel), "ingress_rule": .string("allow_all_ingress"),
                ]))
        ])[0].result
        let port = await bootWeb(kernel)
        let restId = await mountRest(kernel, canCall: .object(["files": .string("*")]))
        _ = await kernel.send([
            Item(
                AgentId("kernel_state"),
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
                AgentId("kernel_state"),
                .object([
                    "type": .string("create_agent"), "parent_id": .string("kernel_state"),
                    "handler_module": .string("file_bridge.tools"), "allow": ["."], "id": .string("files"),
                    "root": .string(rel), "ingress_rule": .string("allow_all_ingress"),
                ]))
        ])[0].result
        let port = await bootWeb(kernel)
        let restId = await mountRest(kernel, canCall: .object(["files": .string("*")]))
        _ = await kernel.send([
            Item(
                AgentId("kernel_state"),
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

    /// THE ROOT `_reflect` SHORTCUT IS THE KERNEL'S IDENTITY, not the loader
    /// bundle's. Python dispatches it at the reserved `kernel` address, which takes the
    /// identity-read fast path (`_agent.py`: `target_id == "kernel" and verb ==
    /// "reflect"` → `_reflect_identity`) — so the sentence is the substrate's,
    /// and `GET /<id>/_reflect/<root>` answers the same. The loader's own
    /// sentence ("Disk persistence/hydration root …") belongs to its verbs, not
    /// to a reflect of the root.
    @Test func restRootReflectIsTheKernelIdentity() async throws {
        let kernel = try await startWebKernel()
        let port = await bootWeb(kernel)
        let restId = await mountRest(kernel)
        defer { Task { await stopWeb(kernel) } }
        let session = URLSession(configuration: .ephemeral)
        for path in ["_reflect", "_reflect/kernel", "_reflect/kernel_state"] {
            let (data, response) = try await session.data(
                from: URL(string: "http://127.0.0.1:\(port)/\(restId)/\(path)")!)
            #expect((response as? HTTPURLResponse)?.statusCode == 200, "\(path)")
            let reply = try JSON.parse(data)
            #expect(
                reply["sentence"].asString
                    == "Fantastic kernel. Everything is reachable by sending messages to agents.",
                "\(path): \(reply["sentence"].serialize())")
            #expect(reply["id"].asString == "kernel_state", "\(path): \(reply)")
        }
    }

    /// D13 — python reads a REST body with `strict_loads` (`io_bridge.loads`):
    /// `NaN`/`Infinity`, an integer outside int64, an overflowing number, a raw
    /// control character and a leading-zero number are NOT VALID JSON — the
    /// parse 400, never a dispatch and never `body must be a JSON object`. Twin
    /// of python `test_post_endpoint_rejects_a_non_finite_number`.
    @Test func webRestStrictBodiesAreNotValidJSON() async throws {
        let kernel = try await startWebKernel()
        let port = await bootWeb(kernel)
        let restId = await mountRest(kernel)
        defer { Task { await stopWeb(kernel) } }
        let session = URLSession(configuration: .ephemeral)
        for body in [
            "NaN", "Infinity", "-Infinity", #"{"type": "reflect", "x": NaN}"#,
            #"{"type": "reflect", "x": 12345678901234567890}"#,
            #"{"type": "reflect", "x": 1e400}"#, #"{"type": "reflect", "x": 01}"#,
            "{\"type\": \"reflect\", \"x\": \"a\u{01}b\"}",
        ] {
            var req = URLRequest(
                url: URL(string: "http://127.0.0.1:\(port)/\(restId)/kernel_state")!)
            req.httpMethod = "POST"
            req.httpBody = Data(body.utf8)
            let (data, response) = try await session.data(for: req)
            #expect((response as? HTTPURLResponse)?.statusCode == 400, "\(body)")
            #expect(
                try JSON.parse(data)["error"].asString
                    == "web_rest: request body is not valid JSON",
                "\(body): \(String(data: data, encoding: .utf8) ?? "")")
        }
    }

    /// `get_routes` over REST, and ONE MEASURED DIVERGENCE FROM PYTHON, PINNED
    /// RATHER THAN HIDDEN.
    ///
    /// • `get_routes` over REST: the descriptors are pure data on both sides
    ///   (python hands its live endpoints over through `route_endpoints(agent)`,
    ///   swift through `WebRouteEndpoints`), so the request renders and answers
    ///   200 with no `endpoint` in any descriptor.
    /// • `/openapi.json`, `/docs`, `/redoc`: a bare `FastAPI()` mounts all three
    ///   (200). The swift host declares python's FOUR routes and 404s the rest,
    ///   so the schema/docs surface does not exist here — a HOST gap
    ///   (`HTTPServer`), not a `web_rest` one.
    @Test func restGetRoutesAndDocsRoutesDivergeFromPython() async throws {
        let kernel = try await startWebKernel()
        let port = await bootWeb(kernel)
        let restId = await mountRest(kernel)
        defer { Task { await stopWeb(kernel) } }
        let session = URLSession(configuration: .ephemeral)
        var req = URLRequest(url: URL(string: "http://127.0.0.1:\(port)/\(restId)/\(restId)")!)
        req.httpMethod = "POST"
        req.httpBody = Data("{\"type\":\"get_routes\"}".utf8)
        let (data, response) = try await session.data(for: req)
        #expect((response as? HTTPURLResponse)?.statusCode == 200)
        let routes = try JSON.parse(data)["routes"].asArray ?? []
        #expect(routes.count == 3, "\(String(data: data, encoding: .utf8) ?? "")")
        #expect(routes.allSatisfy { $0["endpoint"].isNull }, "a descriptor cannot hold a callable")
        for path in ["/openapi.json", "/docs", "/redoc"] {
            let (body, r) = try await session.data(from: URL(string: "http://127.0.0.1:\(port)\(path)")!)
            #expect((r as? HTTPURLResponse)?.statusCode == 404, "\(path) — python: 200")
            #expect(String(data: body, encoding: .utf8) == "{\"detail\":\"Not Found\"}", "\(path)")
        }
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
        // The TASK-TABLE parent too (`ps`), not only the call row's: the table
        // dropped a parent that had already settled, so a REST dispatch
        // reported `parent: null` in `ps` while its row named the boot pid.
        let ps = kernel.tasks.rows().first { $0["id"].asInt == mine?.value }
        return .object([
            "task": mine.map { JSON.integer($0.value) } ?? .null,
            "parent": row?["parent"] ?? .null,
            "ps_parent": ps?["parent"] ?? .null,
        ])
    }
}

/// A `read_stream` SOURCE whose second chunk fails: the first declares a
/// 10-byte file and carries 4 bytes, the next read is refused.
struct ShortSource: AgentBundle {
    let name = "short_source"
    func handle(agentId: AgentId, payload: JSON, kernel: Kernel) async throws -> JSON? { nil }
    func handleBinary(agentId: AgentId, header: JSON, blob: Data, kernel: Kernel)
        async throws -> (JSON?, Data?)
    {
        if header["offset"].asInt == 0 {
            return (
                .object([
                    "path": header["path"], "offset": .integer(0), "bytes": .null,
                    "next_offset": .integer(4), "eof": .bool(false), "size": .integer(10),
                ]), Data([1, 2, 3, 4])
            )
        }
        return (
            .object([
                "error": .string("disk went away"), "reason": .string("transport_error"),
            ]), nil
        )
    }
}
