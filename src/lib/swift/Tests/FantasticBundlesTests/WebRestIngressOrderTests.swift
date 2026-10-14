// web_rest judges a request BEFORE it reads the body, and bounds the body.
//
// Buffering and parsing the WHOLE body before the leg's ingress rule runs would
// let a sealed or password-gated leg make the kernel hold and parse any amount
// of data from a client it is about to refuse. The rule judges the request on what it says up front (its target and its
// `X-Fantastic-Auth` credential), and only an admitted request's body is read —
// at most 16 MiB (io_bridge's `MAX_FRAME`: the one frame cap every wire leg
// holds); past it the request is refused, loud, 413.
//
// Python measures "bytes the app asked for" on its ASGI `receive`; the swift host
// reads the socket itself, so here the measure is WHEN the answer comes: a
// refused or oversize request is answered while the client has NOT finished its
// body (the read deadline is 3 s; a host that waits for the body never answers).
//
// At this leg, a non-ASCII credential is a plain 403.
//
// Twin of python `web_rest/tests/test_web_rest_ingress_order.py`.

import FantasticIoBridge
import FantasticJSON
import FantasticKernel
import FantasticKernelStartup
import Foundation
import OrderedCollections
import Testing

@testable import FantasticWeb

private let SEALED_ERROR = "edge sealed by policy"

/// A `web_rest` leg under `web`, mounted, granted the root (and the root
/// accepting it), with `ingress` as its rule (nil = sealed).
private func mountLeg(_ k: Kernel, _ id: String, ingress: JSON?) async {
    var p: OrderedDictionary<String, JSON> = [
        "type": "create_agent", "parent_id": "web", "handler_module": "web_rest.tools",
        "id": .string(id), "can_call": ["kernel_state": "*"],
    ]
    if let ingress { p["ingress_rule"] = ingress }
    let rec = await k.send([Item("kernel", .object(p))])[0].result
    #expect(rec["id"].asString == id, "\(rec.serialize())")
    var cb: OrderedDictionary<String, JSON> = ["$self": "*", "$parent": "*", "$subtree": "*"]
    cb[id] = "*"
    _ = await k.send([
        Item("kernel", ["type": "update_agent", "id": "kernel_state", "callable_by": .object(cb)])
    ])
    wire(k, "web", id)
    _ = await k.send([Item("web", ["type": "mount", "child_id": .string(id)])])
}

private func boot(_ k: Kernel) async -> UInt16 {
    _ = await k.send([Item("web", ["type": "boot"])])
    return k.httpPort()
}

private func stop(_ k: Kernel) async {
    _ = await k.send([Item("web", ["type": "stop"])])
}

/// The status line's code and the JSON body of one raw response ("" → 0, null).
private func parse(_ raw: String) -> (Int, JSON) {
    let parts = raw.components(separatedBy: "\r\n\r\n")
    let status = Int(parts[0].split(separator: " ").dropFirst().first ?? "") ?? 0
    let body = parts.count > 1 ? (try? JSON.parse(Data(parts[1].utf8))) ?? .null : .null
    return (status, body)
}

private func tooLarge(_ n: Int) -> JSON {
    [
        "error": .string(
            "web_rest: the request body exceeds \(n) bytes — the one frame cap every wire "
                + "leg holds (io_bridge MAX_FRAME); send larger data in chunks (write_stream)"),
        "reason": "transport_error",
        "limit": .integer(Int64(n)),
    ]
}

@Suite("web_rest — the gate before the body", .serialized)
struct WebRestIngressOrderTests {

    @Test func aSealedLegRefusesBeforeReadingTheBody() async throws {
        let k = try await startWebKernel()
        let port = await boot(k)
        defer { Task { await stop(k) } }
        await mountLeg(k, "rs", ingress: nil)
        let sock = try RawHTTPClient(port: port)
        defer { sock.close() }
        // a body DECLARED far longer than what is sent: only an answer that does
        // not wait for it arrives
        try sock.write(
            "POST /rs/kernel_state HTTP/1.1\r\nHost: 127.0.0.1:\(port)\r\ncontent-type: application/json\r\n"
                + "content-length: 100000\r\n\r\n{not json at all ")
        let (status, body) = parse(sock.readSome())
        #expect(status == 403, "\(body.serialize())")
        #expect(body["error"].asString == SEALED_ERROR, "\(body.serialize())")
    }

    @Test func aWrongCredentialIsRefusedBeforeReadingTheBody() async throws {
        let env = "FANTASTIC_GROUP_TOKEN_REST_IO4"
        setenv(env, "grp", 1)
        defer { unsetenv(env) }
        let k = try await startWebKernel()
        let port = await boot(k)
        defer { Task { await stop(k) } }
        await mountLeg(k, "pw", ingress: ["type": "password_ingress", "env": .string(env)])
        let body = Array(#"{"type": "reflect"}"#.utf8)
        // `nope`; `été` as latin-1 bytes; `päss` as utf-8 bytes (IO-6)
        let tokens: [[UInt8]] = [Array("nope".utf8), [0xE9, 0x74, 0xE9], Array("päss".utf8)]
        for token in tokens {
            let sock = try RawHTTPClient(port: port)
            defer { sock.close() }
            try sock.writeBytes(
                Array(
                    ("POST /pw/kernel_state HTTP/1.1\r\nHost: 127.0.0.1:\(port)\r\ncontent-type: application/json\r\n"
                        + "content-length: \(body.count + 1000)\r\nX-Fantastic-Auth: ").utf8)
                    + token + Array("\r\n\r\n".utf8) + body)
            let (status, out) = parse(sock.readSome())
            #expect(status == 403 && out["reason"] == "unauthorized", "\(token) \(out.serialize())")
        }
        let sock = try RawHTTPClient(port: port)
        defer { sock.close() }
        try sock.writeBytes(
            Array(
                ("POST /pw/kernel_state HTTP/1.1\r\nHost: 127.0.0.1:\(port)\r\ncontent-type: application/json\r\n"
                    + "content-length: \(body.count)\r\nX-Fantastic-Auth: grp\r\n\r\n").utf8) + body)
        let (status, out) = parse(sock.readSome())
        #expect(status == 200 && out["id"] == "kernel_state", "\(out.serialize())")
    }

    @Test func theBodyBoundIsTheOneFrameCap() {
        #expect(WebServer.restBodyLimit == MAX_FRAME && MAX_FRAME == 16 * 1024 * 1024)
    }

    @Test func aDeclaredOversizeBodyIsRefusedUnread() async throws {
        let k = try await startWebKernel()
        let port = await boot(k)
        defer { Task { await stop(k) } }
        await mountLeg(k, "ro", ingress: "allow_all_ingress")
        let sock = try RawHTTPClient(port: port)
        defer { sock.close() }
        try sock.write(
            "POST /ro/kernel_state HTTP/1.1\r\nHost: 127.0.0.1:\(port)\r\ncontent-type: application/json\r\n"
                + "content-length: \(MAX_FRAME + 1)\r\n\r\nxxxx")
        let (status, out) = parse(sock.readSome())
        #expect(status == 413, "\(out.serialize())")
        #expect(out == tooLarge(MAX_FRAME), "\(out.serialize())")
    }

    /// No Content-Length (a chunked body): read until the bound is crossed, then
    /// refused — the rest is never waited for.
    @Test func anUndeclaredOversizeBodyStopsAtTheBound() async throws {
        let k = try await startWebKernel()
        let port = await boot(k)
        defer { Task { await stop(k) } }
        await mountLeg(k, "ro", ingress: "allow_all_ingress")
        let sock = try RawHTTPClient(port: port)
        defer { sock.close() }
        try sock.write(
            "POST /ro/kernel_state HTTP/1.1\r\nHost: 127.0.0.1:\(port)\r\ncontent-type: application/json\r\n"
                + "transfer-encoding: chunked\r\n\r\n")
        let mib = 1 << 20
        let chunk = Array(String(mib, radix: 16).utf8) + [13, 10]
            + [UInt8](repeating: 0x79, count: mib) + [13, 10]
        for _ in 0...(MAX_FRAME / mib) { try sock.writeBytes(chunk) }  // 17 MiB, no terminator
        let (status, out) = parse(sock.readSome())
        #expect(status == 413, "\(out.serialize())")
        #expect(out == tooLarge(MAX_FRAME), "\(out.serialize())")
        // an ordinary request still goes through
        let ok = try RawHTTPClient(port: port)
        defer { ok.close() }
        let body = #"{"type": "reflect", "tree": "none"}"#
        try ok.write(
            "POST /ro/kernel_state HTTP/1.1\r\nHost: 127.0.0.1:\(port)\r\ncontent-type: application/json\r\n"
                + "content-length: \(body.utf8.count)\r\n\r\n\(body)")
        let (okStatus, okOut) = parse(ok.readSome())
        #expect(okStatus == 200 && okOut["id"] == "kernel_state", "\(okOut.serialize())")
    }
}
