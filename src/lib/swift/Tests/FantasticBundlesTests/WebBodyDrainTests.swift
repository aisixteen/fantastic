// A route that reads no body never buffers one.
//
// The host's preflight bounds a web_rest POST; every other route — the favicon,
// the file proxy, web_rest's GET `_reflect` — drains a body unread and is served
// on its head. Buffered, a GET with a multi-GB body from a client that reaches
// the port with no credential (the favicon is always mounted and the origin
// guard skips GET) would grow the kernel until it is killed. Python and rust
// never read those bodies.

import FantasticIoBridge
import FantasticJSON
import FantasticKernel
import FantasticKernelStartup
import Foundation
import NIOHTTP1
import OrderedCollections
import Testing

@testable import FantasticWeb

/// A `web_rest` leg `rr` under `web`, open, granted the root (and the root
/// accepting it), mounted.
private func mountRestLeg(_ k: Kernel) async {
    let rec = await k.send([
        Item(
            "kernel",
            [
                "type": "create_agent", "parent_id": "web", "handler_module": "web_rest.tools",
                "id": "rr", "can_call": ["kernel_state": "*"], "ingress_rule": "allow_all_ingress",
            ])
    ])[0].result
    #expect(rec["id"] == "rr", "\(rec.serialize())")
    _ = await k.send([
        Item(
            "kernel",
            [
                "type": "update_agent", "id": "kernel_state",
                "callable_by": ["$self": "*", "$parent": "*", "$subtree": "*", "rr": "*"],
            ])
    ])
    wire(k, "web", "rr")
    _ = await k.send([Item("web", ["type": "mount", "child_id": "rr"])])
}

/// The status line's code of one raw response.
private func status(_ raw: String) -> Int {
    Int(raw.split(separator: " ").dropFirst().first ?? "") ?? 0
}

@Suite("web — a route that reads no body never buffers one", .serialized)
struct WebBodyDrainTests {

    /// THE DECISION, on the head: only a web_rest POST reads a body (bounded);
    /// every other served route drains it.
    @Test func onlyAWebRestPostReadsABody() async throws {
        let k = try await startWebKernel()
        _ = await k.send([Item("web", ["type": "boot"])])
        defer { Task { _ = await k.send([Item("web", ["type": "stop"])]) } }
        await mountRestLeg(k)
        let web = try #require(k.bundles.get("web.tools") as? WebBundle)
        let server = try #require(web.serverFor(AgentId("web")))
        // the server's own authority — any other Host is refused first
        let host = "127.0.0.1:\(k.httpPort())"
        func head(_ method: HTTPMethod, _ uri: String) -> HTTPRequestHead {
            HTTPRequestHead(
                version: .http1_1, method: method, uri: uri,
                headers: ["host": host, "content-length": "\(1 << 30)"])
        }
        for uri in [
            "/_assets/favicon.png", "/nobody/file/a.txt", "/rr/_reflect",
            "/rr/_reflect/kernel_state",
        ] {
            guard case .drain = server.preflight(head(.GET, uri)) else {
                Issue.record("GET \(uri) must be served without its body")
                continue
            }
        }
        let small = HTTPRequestHead(
            version: .http1_1, method: .POST, uri: "/rr/kernel_state",
            headers: ["host": host, "content-length": "2"])
        guard case .read(let limit) = server.preflight(small) else {
            Issue.record("a web_rest POST reads its body")
            return
        }
        #expect(limit == MAX_FRAME)
    }

    /// ON THE WIRE: a body on a route that reads none — even one over the frame
    /// cap — is drained, not refused, the route answers as it would without it,
    /// and the connection stays usable for the next request.
    @Test func aBodyOnAGetRouteIsDrainedAndTheRouteServed() async throws {
        let k = try await startWebKernel()
        _ = await k.send([Item("web", ["type": "boot"])])
        defer { Task { _ = await k.send([Item("web", ["type": "stop"])]) } }
        await mountRestLeg(k)
        let port = k.httpPort()
        // (the favicon on a socket of its own: `readSome` reads a binary body
        // lossily, so it cannot tell where that body ends)
        let icon = try RawHTTPClient(port: port)
        defer { icon.close() }
        try icon.write(
            "GET /_assets/favicon.png HTTP/1.1\r\nHost: 127.0.0.1:\(port)\r\ncontent-length: 5\r\n\r\nhello")
        let favicon = icon.readSome()
        #expect(status(favicon) == 200, "\(favicon.prefix(200))")
        #expect(favicon.contains("content-type: image/png"), "\(favicon.prefix(200))")

        let sock = try RawHTTPClient(port: port)
        defer { sock.close() }
        try sock.write(
            "GET /rr/_reflect/kernel_state HTTP/1.1\r\nHost: 127.0.0.1:\(port)\r\ncontent-length: 3\r\n\r\nxyz")
        let reflect = sock.readSome()
        #expect(status(reflect) == 200, "\(reflect.prefix(300))")
        let body = reflect.components(separatedBy: "\r\n\r\n").dropFirst().joined()
        #expect((try? JSON.parse(Data(body.utf8)))?["id"] == "kernel_state", "\(body.prefix(300))")

        // the same socket — the drained body did not leak into the next request:
        // a body over the frame cap, drained to its end, never a 413
        let over = MAX_FRAME + 1
        try sock.write(
            "GET /nobody/file/a.txt HTTP/1.1\r\nHost: 127.0.0.1:\(port)\r\ncontent-length: \(over)\r\n\r\n")
        let mib = [UInt8](repeating: 0x79, count: 1 << 20)
        var sent = 0
        while sent < over {
            let n = min(mib.count, over - sent)
            try sock.writeBytes(Array(mib[0..<n]))
            sent += n
        }
        let file = sock.readSome()
        #expect(status(file) == 404, "the file proxy answers its own 404: \(file.prefix(200))")
    }
}
