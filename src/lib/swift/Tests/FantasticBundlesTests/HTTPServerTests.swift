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
}
