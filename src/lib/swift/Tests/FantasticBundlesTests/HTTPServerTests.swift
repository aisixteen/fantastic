// 8B: HTTP listener smoke tests.

import FantasticJSON
import FantasticKernel
import FantasticKernelStartup
import FantasticWeb
import Foundation
import Testing

@Suite("HTTP server", .serialized)
struct HTTPServerTests {
    @Test func bootStartsServerOnRandomPort() async throws {
        let kernel = try await startKernelInMemory(portHint: 0)
        let reply = await kernel.send([Item(AgentId("web"), .object(["type": .string("boot")]))])[0].result
        #expect(reply["ok"].asBool == true)
        let port = reply["port"].asInt ?? 0
        #expect(port > 0, "expected non-zero port, got \(port)")
        let kernelPort = kernel.httpPort()
        #expect(kernelPort == UInt16(port))

        // Cleanup.
        _ = await kernel.send([Item(AgentId("web"), .object(["type": .string("shutdown")]))])[0].result
    }

    @Test func servesIndexRoute() async throws {
        let kernel = try await startKernelInMemory(portHint: 0)
        _ = await kernel.send([Item(AgentId("web"), .object(["type": .string("boot")]))])[0].result
        defer {
            Task {
                _ = await kernel.send([Item(AgentId("web"), .object(["type": .string("shutdown")]))])[0].result
            }
        }
        let port = kernel.httpPort()
        let url = URL(string: "http://127.0.0.1:\(port)/")!
        let (data, response) = try await URLSession.shared.data(from: url)
        let http = try #require(response as? HTTPURLResponse)
        #expect(http.statusCode == 200)
        let html = String(data: data, encoding: .utf8) ?? ""
        #expect(html.contains("fantastic kernel"))
        #expect(html.contains("core"))
    }

    @Test func servesTransportJS() async throws {
        let kernel = try await startKernelInMemory(portHint: 0)
        _ = await kernel.send([Item(AgentId("web"), .object(["type": .string("boot")]))])[0].result
        defer {
            Task {
                _ = await kernel.send([Item(AgentId("web"), .object(["type": .string("shutdown")]))])[0].result
            }
        }
        let port = kernel.httpPort()
        let url = URL(string: "http://127.0.0.1:\(port)/_fantastic/transport.js")!
        let (data, response) = try await URLSession.shared.data(from: url)
        let http = try #require(response as? HTTPURLResponse)
        #expect(http.statusCode == 200)
        #expect(http.value(forHTTPHeaderField: "Content-Type")?.contains("javascript") == true)
        #expect(data.count > 100, "transport.js should be non-trivial")
    }

    @Test func servesAssetWithImmutableCache() async throws {
        let kernel = try await startKernelInMemory(portHint: 0)
        _ = await kernel.send([Item(AgentId("web"), .object(["type": .string("boot")]))])[0].result
        defer {
            Task {
                _ = await kernel.send([Item(AgentId("web"), .object(["type": .string("shutdown")]))])[0].result
            }
        }
        let port = kernel.httpPort()
        let url = URL(string: "http://127.0.0.1:\(port)/_assets/three.module.js")!
        let (data, response) = try await URLSession.shared.data(from: url)
        let http = try #require(response as? HTTPURLResponse)
        #expect(http.statusCode == 200)
        let cache = http.value(forHTTPHeaderField: "Cache-Control") ?? ""
        #expect(cache.contains("immutable"))
        #expect(data.count > 100_000)
    }

    @Test func unknownAssetReturns404() async throws {
        let kernel = try await startKernelInMemory(portHint: 0)
        _ = await kernel.send([Item(AgentId("web"), .object(["type": .string("boot")]))])[0].result
        defer {
            Task {
                _ = await kernel.send([Item(AgentId("web"), .object(["type": .string("shutdown")]))])[0].result
            }
        }
        let port = kernel.httpPort()
        let url = URL(string: "http://127.0.0.1:\(port)/_assets/nope.js")!
        let (_, response) = try await URLSession.shared.data(from: url)
        let http = try #require(response as? HTTPURLResponse)
        #expect(http.statusCode == 404)
    }

    @Test func servesFileAgentContent() async throws {
        // Post-decoupling the host renders no HTML itself; UI is served
        // statically from a `file` agent (the same recipe that serves the
        // TS frontend's dist/). Prove the generic /<id>/file/<path> route
        // returns a file agent's bytes.
        let kernel = try await startKernelInMemory(portHint: 0)
        // The fs edge clamps every root INSIDE the running dir (here the process
        // cwd, since the kernel is in-memory). Serve from a cwd-relative dir —
        // an absolute /tmp root would now refuse (the running-dir law).
        let rel = "fantastic-serve-\(UUID().uuidString)"
        let tmp = URL(fileURLWithPath: FileManager.default.currentDirectoryPath)
            .appendingPathComponent(rel)
        try? FileManager.default.createDirectory(at: tmp, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: tmp) }
        try "<h1>Hello</h1>".write(
            to: tmp.appendingPathComponent("index.html"), atomically: true, encoding: .utf8)
        _ = await kernel.send([Item(AgentId("core"), .object([
                "type": .string("create_agent"),
                "parent_id": .string("core"),
                "handler_module": .string("file_bridge.tools"),
                "id": .string("assets"),
                "root": .string(rel),
                // The fs edge SEALS by default — open it for the /file/ proxy.
                "ingress_rule": .string("allow_all_ingress"),
            ]))])[0].result
        _ = await kernel.send([Item(AgentId("web"), .object(["type": .string("boot")]))])[0].result
        defer {
            Task {
                _ = await kernel.send([Item(AgentId("web"), .object(["type": .string("shutdown")]))])[0].result
            }
        }
        let port = kernel.httpPort()
        let url = URL(string: "http://127.0.0.1:\(port)/assets/file/index.html")!
        let (data, response) = try await URLSession.shared.data(from: url)
        let http = try #require(response as? HTTPURLResponse)
        #expect(http.statusCode == 200)
        let body = String(data: data, encoding: .utf8) ?? ""
        #expect(body.contains("<h1>Hello</h1>"))
    }

    @Test func webRestPostDispatchesBodyVerb() async throws {
        // REST moved out of the host into the `web_rest` child (parity
        // with Python): POST /<rest_id>/<target> with the verb in the
        // BODY. Seed a web_rest child + mount it, then dispatch.
        let kernel = try await startKernelInMemory(portHint: 0)
        _ = await kernel.send([Item(AgentId("web"), .object(["type": .string("boot")]))])[0].result
        let rec = await kernel.send([Item(AgentId("kernel"), .object([
                "type": .string("create_agent"),
                "parent_id": .string("web"),
                "handler_module": .string("web_rest.tools"),
                "id": .string("rest"),
                // The web_rest leg SEALS by default — open it for the POST.
                "ingress_rule": .string("allow_all_ingress"),
                // Deny-all: an inbound relay dispatches browser calls into the
                // kernel AS ITSELF — ingress_rule says WHO may call in, can_call
                // says WHERE into the kernel it lands. This test POSTs to `core`.
                "can_call": .object(["core": .string("*")]),
            ]))])[0].result
        let restId = rec["id"].asString ?? "rest"
        // Two-sided deny-all: the relay dispatches into root AS ITSELF, but it is
        // root's GRANDCHILD (child of `web`), so $children:* does not cover it — open
        // root's ingress for the relay (merged with the structural baseline).
        _ = await kernel.send([Item(AgentId("core"), .object([
                "type": .string("update_agent"), "id": .string("core"),
                "callable_by": .object([
                    "$self": .string("*"), "$parent": .string("*"),
                    "$children": .string("*"), restId: .string("*"),
                ]),
            ]))])[0].result
        _ = await kernel.send([Item(AgentId("web"), .object(["type": .string("mount"), "child_id": .string(restId)]))])[0].result
        defer {
            Task {
                _ = await kernel.send([Item(AgentId("web"), .object(["type": .string("shutdown")]))])[0].result
            }
        }
        let port = kernel.httpPort()
        var req = URLRequest(
            url: URL(string: "http://127.0.0.1:\(port)/\(restId)/core")!)
        req.httpMethod = "POST"
        req.setValue("application/json", forHTTPHeaderField: "Content-Type")
        req.httpBody = "{\"type\":\"list_agents\"}".data(using: .utf8)
        let (data, response) = try await URLSession.shared.data(for: req)
        let http = try #require(response as? HTTPURLResponse)
        #expect(http.statusCode == 200)
        let json = try JSON.parse(data)
        let ids = (json["agents"].asArray ?? []).compactMap { $0["id"].asString }
        #expect(ids.contains("core"))
    }

    @Test func webRestBinaryLegPatchAndReadStream() async throws {
        // The BINARY leg (parity with py web_rest): a verb whose payload OR
        // reply carries raw bytes rides an octet-stream body — the raw body IS
        // the blob, the rest of the envelope on `X-Fantastic-Payload`, the
        // bytes field named by `X-Fantastic-Bytes-Field` (default `bytes`).
        // Never base64. Here: patch_text_file (patch in the body → JSON
        // {patched:true}) and read_stream (octet reply = the file bytes).
        let kernel = try await startKernelInMemory(portHint: 0)
        _ = await kernel.send([Item(AgentId("web"), .object(["type": .string("boot")]))])[0].result

        // A file agent rooted inside cwd (the running-dir clamp) holding a text
        // file to patch + read.
        let rel = "fantastic-binleg-\(UUID().uuidString)"
        let dir = URL(fileURLWithPath: FileManager.default.currentDirectoryPath)
            .appendingPathComponent(rel)
        try? FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: dir) }
        try "hello world\n".write(
            to: dir.appendingPathComponent("note.txt"), atomically: true, encoding: .utf8)

        _ = await kernel.send([Item(AgentId("core"), .object([
                "type": .string("create_agent"),
                "parent_id": .string("core"),
                "handler_module": .string("file_bridge.tools"),
                "id": .string("files"),
                "root": .string(rel),
                "ingress_rule": .string("allow_all_ingress"),
            ]))])[0].result

        // web_rest leg, opened, allowed to reach the file agent.
        let rec = await kernel.send([Item(AgentId("kernel"), .object([
                "type": .string("create_agent"),
                "parent_id": .string("web"),
                "handler_module": .string("web_rest.tools"),
                "id": .string("rest"),
                "ingress_rule": .string("allow_all_ingress"),
                "can_call": .object(["files": .string("*")]),
            ]))])[0].result
        let restId = rec["id"].asString ?? "rest"
        // The file agent must accept the leg (two-sided gate): it is root's
        // grandchild, so $children:* does not cover it — open its ingress.
        _ = await kernel.send([Item(AgentId("core"), .object([
                "type": .string("update_agent"), "id": .string("files"),
                "callable_by": .object([
                    "$self": .string("*"), "$parent": .string("*"),
                    "$children": .string("*"), restId: .string("*"),
                ]),
            ]))])[0].result
        _ = await kernel.send([Item(AgentId("web"), .object(["type": .string("mount"), "child_id": .string(restId)]))])[0].result
        defer {
            Task {
                _ = await kernel.send([Item(AgentId("web"), .object(["type": .string("shutdown")]))])[0].result
            }
        }
        let port = kernel.httpPort()
        // A dedicated ephemeral session — no shared connection pool with the
        // sibling suites that also drive URLSession, which is the shared state
        // that makes real-socket tests flaky under parallelism.
        let session = URLSession(configuration: .ephemeral)
        defer { session.finishTasksAndInvalidate() }

        // ── patch_text_file: patch rides the octet body, JSON envelope on the
        //    header → JSON reply {patched:true}. ──
        let patch = "<<<<<<< SEARCH\nhello world\n=======\ngoodbye world\n>>>>>>> REPLACE\n"
        var patchReq = URLRequest(url: URL(string: "http://127.0.0.1:\(port)/\(restId)/files")!)
        patchReq.httpMethod = "POST"
        patchReq.setValue("application/octet-stream", forHTTPHeaderField: "Content-Type")
        patchReq.setValue(
            "{\"type\":\"patch_text_file\",\"path\":\"note.txt\"}",
            forHTTPHeaderField: "X-Fantastic-Payload")
        patchReq.setValue("bytes", forHTTPHeaderField: "X-Fantastic-Bytes-Field")
        patchReq.httpBody = patch.data(using: .utf8)
        let (patchData, patchResp) = try await session.data(for: patchReq)
        let patchHTTP = try #require(patchResp as? HTTPURLResponse)
        #expect(patchHTTP.statusCode == 200)
        #expect(
            (patchHTTP.value(forHTTPHeaderField: "Content-Type") ?? "").contains(
                "application/json"))
        let patchJSON = try JSON.parse(patchData)
        #expect(patchJSON["patched"].asBool == true)

        // ── read_stream: octet REPLY whose body is the file bytes; the rest of
        //    the reply envelope rides X-Fantastic-Payload. ──
        var readReq = URLRequest(url: URL(string: "http://127.0.0.1:\(port)/\(restId)/files")!)
        readReq.httpMethod = "POST"
        readReq.setValue("application/octet-stream", forHTTPHeaderField: "Content-Type")
        readReq.setValue(
            "{\"type\":\"read_stream\",\"path\":\"note.txt\"}",
            forHTTPHeaderField: "X-Fantastic-Payload")
        let (readData, readResp) = try await session.data(for: readReq)
        let readHTTP = try #require(readResp as? HTTPURLResponse)
        #expect(readHTTP.statusCode == 200)
        #expect(
            (readHTTP.value(forHTTPHeaderField: "Content-Type") ?? "").contains(
                "application/octet-stream"))
        // The body IS the (patched) file bytes — never base64.
        #expect(String(data: readData, encoding: .utf8) == "goodbye world\n")
        // The rest of the reply envelope rode the header.
        #expect(readHTTP.value(forHTTPHeaderField: "X-Fantastic-Bytes-Field") == "bytes")
        let readMeta = try JSON.parse(
            try #require(readHTTP.value(forHTTPHeaderField: "X-Fantastic-Payload")))
        #expect(readMeta["eof"].asBool == true)
        #expect(readMeta["size"].asInt == Int64("goodbye world\n".utf8.count))
    }
}
