// Twins of python `web/host/tests/test_origin_guard.py` (2026-09-24): the web
// host binds LOOPBACK unless its record names a `host`, and refuses a FOREIGN
// browser origin on websocket upgrades and non-GET/HEAD/OPTIONS HTTP requests
// unless the record's `allowed_origins` lists it.

import FantasticJSON
import FantasticKernel
import FantasticKernelStartup
import Foundation
import Testing

@testable import FantasticWeb

@Suite("WebOriginGuard", .serialized)
struct WebOriginGuardTests {
    private func call(_ kernel: Kernel, _ id: String, _ payload: JSON) async -> JSON {
        await kernel.send([Item(AgentId(id), payload)])[0].result
    }

    @Test func loopbackIsTheDefaultAndItsSpellingsAreOwn() {
        #expect(WEB_DEFAULT_HOST == "127.0.0.1")
        let own = webOwnOrigins(.object(["port": .integer(8088)]))
        #expect(own.isSuperset(of: ["http://127.0.0.1:8088", "http://localhost:8088"]))
        #expect(
            webOwnOrigins(.object(["host": .string("0.0.0.0"), "port": .integer(8088)]))
                == ["http://0.0.0.0:8088"])
    }

    @Test func originRule() {
        let rec: JSON = .object(["port": .integer(8088)])
        #expect(webOriginAllowed(rec, nil))
        #expect(webOriginAllowed(rec, "http://localhost:8088"))
        #expect(!webOriginAllowed(rec, "http://evil.example:8088"))
        #expect(!webOriginAllowed(rec, "http://localhost:9999"))
        var listed = rec
        listed["allowed_origins"] = .array([.string("https://ui.example")])
        #expect(webOriginAllowed(listed, "https://ui.example"))
        listed["allowed_origins"] = .string("https://ui.example")  // `k=v` yields a string
        #expect(webOriginAllowed(listed, "https://ui.example"))
    }

    private func request(
        _ port: UInt16, _ method: String, _ origin: String?
    ) async throws -> (Int, Data) {
        var req = URLRequest(url: URL(string: "http://127.0.0.1:\(port)/")!)
        req.httpMethod = method
        if let origin { req.setValue(origin, forHTTPHeaderField: "Origin") }
        let (data, resp) = try await URLSession(configuration: .ephemeral).data(for: req)
        return ((resp as! HTTPURLResponse).statusCode, data)
    }

    @Test func foreignPostIs403OwnAndOriginlessPassGetIsNotGated() async throws {
        let kernel = try await startWebKernel()
        defer { Task { _ = await call(kernel, "web", .object(["type": .string("stop")])) } }
        let port = kernel.httpPort()
        let (status, body) = try await request(port, "POST", "http://evil.example")
        #expect(status == 403)
        #expect(
            String(decoding: body, as: UTF8.self)
                == "{\"error\": \"origin 'http://evil.example' is not allowed on this host\", "
                + "\"reason\": \"unauthorized\", \"hint\": \"add it to the web record's allowed_origins\"}")
        // Past the guard, `/` is GET-only → the router's 405, not the guard's 403.
        #expect(try await request(port, "POST", "http://localhost:\(port)").0 == 405)
        #expect(try await request(port, "POST", nil).0 == 405)
        #expect(try await request(port, "GET", "http://evil.example").0 == 200)
        let r = await call(kernel, "web", .object(["type": .string("reflect")]))
        #expect(r["host"].asString == "127.0.0.1")
        #expect(
            r["allowed_origins"].asArray?.compactMap { $0.asString }
                == ["http://127.0.0.1:\(port)", "http://[::1]:\(port)", "http://localhost:\(port)"])
    }

    private func wsOpens(_ port: UInt16, origin: String) async -> Bool {
        var req = URLRequest(url: URL(string: "ws://127.0.0.1:\(port)/kernel_state/ws")!)
        req.setValue(origin, forHTTPHeaderField: "Origin")
        let task = URLSession(configuration: .ephemeral).webSocketTask(with: req)
        task.resume()
        defer { task.cancel(with: .normalClosure, reason: nil) }
        do {
            try await task.send(
                .string(
                    JSON.object([
                        "type": .string("call"), "id": .string("o1"),
                        "target": .string("kernel_state"),
                        "payload": .object(["type": .string("list_agents")]),
                    ]).serialize()))
            _ = try await task.receive()
            return true
        } catch {
            return false
        }
    }

    @Test func foreignWebsocketUpgradeIsRefusedListedOriginPasses() async throws {
        let kernel = try await startWebKernel()
        defer { Task { _ = await call(kernel, "web", .object(["type": .string("stop")])) } }
        let port = kernel.httpPort()
        _ = await call(
            kernel, "kernel",
            .object([
                "type": .string("create_agent"), "parent_id": .string("web"),
                "handler_module": .string("web_ws.tools"), "id": .string("web_ws"),
                "ingress_rule": .string("allow_all_ingress"),
                "can_call": .object(["kernel_state": .string("*")]),
            ]))
        _ = await call(
            kernel, "kernel_state",
            .object([
                "type": .string("update_agent"), "id": .string("kernel_state"),
                "callable_by": .object([
                    "$self": .string("*"), "$parent": .string("*"), "$children": .string("*"),
                    "web_ws": .string("*"),
                ]),
            ]))
        _ = await call(kernel, "web", .object(["type": .string("mount"), "child_id": .string("web_ws")]))
        // Read PER REQUEST: the edit applies without a restart.
        _ = await call(
            kernel, "kernel_state",
            .object([
                "type": .string("update_agent"), "id": .string("web"),
                "allowed_origins": .array([.string("https://ui.example")]),
            ]))
        #expect(await wsOpens(port, origin: "http://evil.example") == false)
        #expect(await wsOpens(port, origin: "https://ui.example") == true)
    }
}
