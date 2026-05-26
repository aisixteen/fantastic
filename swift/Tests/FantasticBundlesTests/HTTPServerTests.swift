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
        let reply = await kernel.send(
            AgentId("web"), .object(["type": .string("boot")]))
        #expect(reply["ok"].asBool == true)
        let port = reply["port"].asInt ?? 0
        #expect(port > 0, "expected non-zero port, got \(port)")
        let kernelPort = kernel.httpPort()
        #expect(kernelPort == UInt16(port))

        // Cleanup.
        _ = await kernel.send(
            AgentId("web"), .object(["type": .string("shutdown")]))
    }

    @Test func servesIndexRoute() async throws {
        let kernel = try await startKernelInMemory(portHint: 0)
        _ = await kernel.send(
            AgentId("web"), .object(["type": .string("boot")]))
        defer {
            Task {
                _ = await kernel.send(
                    AgentId("web"), .object(["type": .string("shutdown")]))
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
        _ = await kernel.send(
            AgentId("web"), .object(["type": .string("boot")]))
        defer {
            Task {
                _ = await kernel.send(
                    AgentId("web"), .object(["type": .string("shutdown")]))
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
        _ = await kernel.send(
            AgentId("web"), .object(["type": .string("boot")]))
        defer {
            Task {
                _ = await kernel.send(
                    AgentId("web"), .object(["type": .string("shutdown")]))
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
        _ = await kernel.send(
            AgentId("web"), .object(["type": .string("boot")]))
        defer {
            Task {
                _ = await kernel.send(
                    AgentId("web"), .object(["type": .string("shutdown")]))
            }
        }
        let port = kernel.httpPort()
        let url = URL(string: "http://127.0.0.1:\(port)/_assets/nope.js")!
        let (_, response) = try await URLSession.shared.data(from: url)
        let http = try #require(response as? HTTPURLResponse)
        #expect(http.statusCode == 404)
    }

    @Test func servesAgentRenderHtml() async throws {
        let kernel = try await startKernelInMemory(portHint: 0)
        _ = await kernel.send(
            AgentId("core"),
            .object([
                "type": .string("create_agent"),
                "handler_module": .string("html_agent.tools"),
                "id": .string("hi"),
                "html": .string("<h1>Hello</h1>"),
            ]))
        _ = await kernel.send(
            AgentId("web"), .object(["type": .string("boot")]))
        defer {
            Task {
                _ = await kernel.send(
                    AgentId("web"), .object(["type": .string("shutdown")]))
            }
        }
        let port = kernel.httpPort()
        let url = URL(string: "http://127.0.0.1:\(port)/hi/")!
        let (data, response) = try await URLSession.shared.data(from: url)
        let http = try #require(response as? HTTPURLResponse)
        #expect(http.statusCode == 200)
        let body = String(data: data, encoding: .utf8) ?? ""
        #expect(body.contains("<h1>Hello</h1>"))
        // transport.js auto-injected at the Python-matching URL.
        #expect(body.contains("/_fantastic/transport.js"))
    }

    @Test func acceptsPostVerbToAgent() async throws {
        let kernel = try await startKernelInMemory(portHint: 0)
        _ = await kernel.send(
            AgentId("web"), .object(["type": .string("boot")]))
        defer {
            Task {
                _ = await kernel.send(
                    AgentId("web"), .object(["type": .string("shutdown")]))
            }
        }
        let port = kernel.httpPort()
        var req = URLRequest(url: URL(string: "http://127.0.0.1:\(port)/core/list_agents")!)
        req.httpMethod = "POST"
        req.setValue("application/json", forHTTPHeaderField: "Content-Type")
        req.httpBody = "{}".data(using: .utf8)
        let (data, response) = try await URLSession.shared.data(for: req)
        let http = try #require(response as? HTTPURLResponse)
        #expect(http.statusCode == 200)
        let json = try JSON.parse(data)
        let ids = (json["agents"].asArray ?? []).compactMap { $0["id"].asString }
        #expect(ids.contains("core"))
    }
}
