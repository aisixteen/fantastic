// Twins of python `web/host/tests/test_origin_guard.py`: the web
// host binds LOOPBACK unless its record names a `host`, refuses a FOREIGN
// browser origin on websocket upgrades and non-GET/HEAD/OPTIONS HTTP requests
// unless the record's `allowed_origins` lists it, and refuses EVERY
// request whose Host is not the host's own authority.

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
        var req = URLRequest(url: URL(string: "http://127.0.0.1:\(port)/_assets/favicon.png")!)
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
        // Past the guard, the favicon is GET-only → the router's 405, not the guard's 403.
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
                    "$self": .string("*"), "$parent": .string("*"), "$subtree": .string("*"),
                    "web_ws": .string("*"),
                ]),
            ]))
        wire(kernel, "web", "web_ws")
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

    // MARK: - the Host guard

    @Test func ownAuthoritiesAreTheBoundHostPortPlusLoopbackSpellings() {
        let loopback: Set<String> = ["127.0.0.1:8088", "localhost:8088", "[::1]:8088"]
        #expect(webOwnAuthorities(.object(["port": .integer(8088)])) == loopback)
        #expect(
            webOwnAuthorities(.object(["host": .string("localhost"), "port": .integer(8088)]))
                == loopback)
        // a non-loopback bind answers its own host:port and nothing else
        #expect(
            webOwnAuthorities(.object(["host": .string("0.0.0.0"), "port": .integer(8088)]))
                == ["0.0.0.0:8088"])
        #expect(
            webOwnAuthorities(.object(["host": .string("192.168.1.5"), "port": .integer(8088)]))
                == ["192.168.1.5:8088"])
        // an IPv6 literal is bracketed, as a Host header spells it
        #expect(
            webOwnAuthorities(.object(["host": .string("::"), "port": .integer(8088)]))
                == ["[::]:8088"])
        // no Host header at all is not an authority
        #expect(!webOwnAuthorities(.object(["port": .integer(8088)])).contains(""))
    }

    /// One raw request naming `host` (URLSession will not send a foreign Host).
    private func raw(
        _ port: UInt16, _ method: String, _ path: String, host: String
    ) throws -> String {
        let sock = try RawHTTPClient(port: port)
        defer { sock.close() }
        try sock.write("\(method) \(path) HTTP/1.1\r\nHost: \(host)\r\nConnection: close\r\n\r\n")
        return sock.readSome()
    }

    /// THE REBINDING READ: a page whose domain rebinds to 127.0.0.1 sends a
    /// same-origin GET — no Origin header — naming its own domain in Host.
    @Test func aReboundHostIsRefusedOnEveryMethodWithNoOrigin() async throws {
        let kernel = try await startWebKernel()
        defer { Task { _ = await call(kernel, "web", .object(["type": .string("stop")])) } }
        let port = kernel.httpPort()
        let rebound = "rebind.invalid:\(port)"
        let got = try raw(port, "GET", "/_assets/favicon.png", host: rebound)
        #expect(got.hasPrefix("HTTP/1.1 403 "), "\(got)")
        #expect(
            got.hasSuffix(
                "{\"error\": \"host '\(rebound)' is not this server's own authority\", "
                    + "\"reason\": \"unauthorized\", \"hint\": \"address the host by its bound "
                    + "host:port (a loopback bind also answers localhost, 127.0.0.1 and [::1] "
                    + "on that port)\"}"),
            "\(got)")
        for method in ["HEAD", "OPTIONS", "POST"] {
            let r = try raw(port, method, "/_assets/favicon.png", host: rebound)
            #expect(r.hasPrefix("HTTP/1.1 403 "), "\(method): \(r)")
        }
        // an own authority passes the guard: the router answers (`/nope` is its 404)
        for host in ["127.0.0.1:\(port)", "localhost:\(port)", "[::1]:\(port)"] {
            let r = try raw(port, "GET", "/nope", host: host)
            #expect(r.hasPrefix("HTTP/1.1 404 "), "\(host): \(r)")
        }
        for host in ["127.0.0.1:\(port &+ 1)", "localhost", "127.0.0.1"] {
            let r = try raw(port, "GET", "/_assets/favicon.png", host: host)
            #expect(r.hasPrefix("HTTP/1.1 403 "), "\(host): \(r)")
        }
    }

    /// `0.0.0.0` publishes the port, but the only Host it answers is
    /// `0.0.0.0:<port>` — a port map, a LAN name, a tunnel name, even the loopback
    /// spellings are other authorities. Judged on the
    /// request head, without binding a public port.
    @Test func aPublishedBindAdmitsOnlyItsOwnHostPort() async throws {
        let kernel = try await startKernelInMemory(portHint: 0)
        _ = await call(
            kernel, "kernel_state",
            .object([
                "type": .string("update_agent"), "id": .string("web"),
                "host": .string("0.0.0.0"), "port": .integer(8088),
            ]))
        let server = WebServer(kernel: kernel, agentId: AgentId("web"), port: 8088)
        func status(_ host: String) -> Int? {
            if case .respond(let r) = server.resolve(
                method: "GET", path: "/_assets/favicon.png", headers: ["host": host])
            {
                return r.status
            }
            return nil
        }
        #expect(status("0.0.0.0:8088") == nil)
        for host in ["127.0.0.1:8088", "localhost:8088", "192.168.1.5:8088", "box.lan:8088"] {
            #expect(status(host) == 403, "\(host)")
        }
    }

    @Test func aReboundWebsocketUpgradeIsRefused() async throws {
        let kernel = try await startWebKernel()
        defer { Task { _ = await call(kernel, "web", .object(["type": .string("stop")])) } }
        let port = kernel.httpPort()
        _ = await call(
            kernel, "kernel",
            .object([
                "type": .string("create_agent"), "parent_id": .string("web"),
                "handler_module": .string("web_ws.tools"), "id": .string("web_ws"),
                "ingress_rule": .string("allow_all_ingress"),
            ]))
        wire(kernel, "web", "web_ws")
        _ = await call(
            kernel, "web", .object(["type": .string("mount"), "child_id": .string("web_ws")]))
        func upgrade(_ host: String) throws -> String {
            let sock = try RawHTTPClient(port: port)
            defer { sock.close() }
            try sock.write(
                "GET /kernel_state/ws HTTP/1.1\r\nHost: \(host)\r\nUpgrade: websocket\r\n"
                    + "Connection: Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n"
                    + "Sec-WebSocket-Version: 13\r\n\r\n")
            return sock.readSome()
        }
        let refused = try upgrade("rebind.invalid:\(port)")
        #expect(refused.hasPrefix("HTTP/1.1 403 "), "\(refused)")
        let own = try upgrade("127.0.0.1:\(port)")
        #expect(own.hasPrefix("HTTP/1.1 101 "), "\(own)")
    }
}
