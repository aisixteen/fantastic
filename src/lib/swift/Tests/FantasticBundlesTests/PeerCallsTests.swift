// The web ws leg as the TRANSPORT of cross-kernel addressing: a page says
// `hello {guid}`, the host kernel then reaches the page's agents as
// `<guid>|<agent>`, each call rides this socket as an outbound `call` frame
// answered by an inbound `reply`/`error` with the same id, and the page stops
// being an address the moment its socket closes (owner, 2026-09-30).
//
// The twin of python `web_ws/tests/test_peer_calls.py`, over a real socket: the
// test is the page, and sends INTO it from the kernel it is connected to.

import FantasticIoBridge
import FantasticJSON
import FantasticKernel
import FantasticKernelStartup
import Foundation
import OrderedCollections
import Testing

@testable import FantasticWeb

private let HOST = "h0"
private let PAGE = "g1"

/// A booted web host, its `leg` web_ws leg mounted (open, or sealed with
/// `deny_ingress`), and the root's guid set to `HOST`.
private func webKernel(ingress: String = "allow_all_ingress") async throws -> Kernel {
    let k = try await startWebKernel()
    _ = await k.send([Item(AgentId("web"), .object(["type": .string("boot")]))])[0].result
    k.root?.updateMeta(["guid": .string(HOST)])
    let out = await k.send([
        Item(
            AgentId("kernel"),
            .object([
                "type": .string("create_agent"), "parent_id": .string("web"),
                "handler_module": .string("web_ws.tools"), "id": .string("leg"),
                "ingress_rule": .string(ingress),
            ]))
    ])[0].result
    #expect(out["id"].asString == "leg", "\(out.serialize())")
    _ = await k.send([
        Item(AgentId("web"), .object(["type": .string("mount"), "child_id": .string("leg")]))
    ])[0].result
    return k
}

private func stop(_ k: Kernel) async {
    _ = await k.send([Item(AgentId("web"), .object(["type": .string("stop")]))])[0].result
}

/// A socket on the WEB agent — the leg's own parent, which its baseline lets it
/// watch, so the connect-time auto-watch adds no error frame to the stream.
private func socket(_ k: Kernel) -> URLSessionWebSocketTask {
    let task = URLSession(configuration: .ephemeral)
        .webSocketTask(with: URL(string: "ws://127.0.0.1:\(k.httpPort())/web/ws")!)
    task.resume()
    return task
}

/// The next frame of type `ftype`, BOUNDED: `URLSessionWebSocketTask.receive`
/// ignores task cancellation, so past `seconds` the socket is cancelled, which
/// makes the pending receive throw — a missing frame is a failure, not a stall.
/// A binary frame comes back as its header plus the body.
private func next(
    _ task: URLSessionWebSocketTask, _ ftype: String, seconds: UInt64 = 5
) async throws -> (JSON, Data?) {
    let timer = Task {
        try await Task.sleep(nanoseconds: seconds * 1_000_000_000)
        task.cancel(with: .goingAway, reason: nil)
    }
    defer { timer.cancel() }
    while true {
        let frame: (JSON, Data?)
        switch try await task.receive() {
        case .string(let s): frame = (try JSON.parse(s), nil)
        case .data(let d):
            let (h, b) = try Codec.decodeBinaryFrame(d)
            frame = (h, b)
        @unknown default: continue
        }
        if frame.0["type"].asString == ftype { return frame }
    }
}

private func push(_ task: URLSessionWebSocketTask, _ frame: JSON) async throws {
    try await task.send(.string(frame.serialize()))
}

private func hello(_ guid: String) -> JSON {
    .object(["type": .string("hello"), "guid": .string(guid)])
}

/// python `_until`: poll `pred` for up to 3 s.
private func until(_ what: String, _ pred: () -> Bool) async throws {
    let end = Date().addingTimeInterval(3)
    while !pred() {
        if Date() >= end {
            Issue.record("\(what): not reached")
            return
        }
        try await Task.sleep(nanoseconds: 10_000_000)
    }
}

private func sendToPage(_ k: Kernel, _ payload: JSON, binary: Blob? = nil) -> Task<Outcome, Never>
{
    var item = Item(AgentId("\(PAGE)|canvas"), payload)
    item.binary = binary
    return Task { await k.send([item])[0] }
}

private func around() -> JSON { .object(["type": .string("around")]) }

@Suite("web_ws — the page as a peer kernel", .serialized)
struct PeerCallsTests {
    @Test(.timeLimit(.minutes(1)))
    func helloMakesThePageAnAddressAndACallRoundTrips() async throws {
        let k = try await webKernel()
        let ws = socket(k)
        try await push(ws, hello(PAGE))
        try await until("peer registered") { !k.peerRoutes(PAGE).isEmpty }
        #expect(
            k.peerRows() == .array([.object(["guid": .string(PAGE), "via": .string("leg")])]))
        let payload: JSON = .object(["type": .string("around"), "radius": .integer(2)])
        let send = sendToPage(k, payload)
        let (call, _) = try await next(ws, "call")
        #expect(call["target"].asString == "canvas", "\(call.serialize())")
        #expect(call["payload"] == payload, "\(call.serialize())")
        #expect(call["caller"].asString == "\(HOST)|kernel", "\(call.serialize())")
        try await push(
            ws,
            .object([
                "type": .string("reply"), "id": call["id"],
                "data": .object(["cells": .array([.string("a")])]),
            ]))
        let out = await send.value
        #expect(out.status == .completed, "\(out.result.serialize())")
        #expect(out.result == .object(["cells": .array([.string("a")])]))
        ws.cancel(with: .normalClosure, reason: nil)
        await stop(k)
    }

    @Test(.timeLimit(.minutes(1)))
    func aPageSideRefusalIsTheCallsOutcome() async throws {
        let k = try await webKernel()
        let ws = socket(k)
        try await push(ws, hello(PAGE))
        try await until("peer registered") { !k.peerRoutes(PAGE).isEmpty }
        let send = sendToPage(k, around())
        let (call, _) = try await next(ws, "call")
        let refusal: JSON = .object([
            "error": .string("canvas does not accept 'around' from h0|kernel"),
            "reason": .string("unauthorized"),
        ])
        try await push(ws, .object(["type": .string("error"), "id": call["id"], "data": refusal]))
        let out = await send.value
        #expect(out.result == refusal, "\(out.result.serialize())")
        ws.cancel(with: .normalClosure, reason: nil)
        await stop(k)
    }

    @Test(.timeLimit(.minutes(1)))
    func aDisconnectSettlesWaitingCallsAndDropsTheAddress() async throws {
        let k = try await webKernel()
        let ws = socket(k)
        try await push(ws, hello(PAGE))
        try await until("peer registered") { !k.peerRoutes(PAGE).isEmpty }
        let send = sendToPage(k, around())
        _ = try await next(ws, "call")
        ws.cancel(with: .normalClosure, reason: nil)
        let out = await send.value
        #expect(out.result["reason"].asString == "transport_error", "\(out.result.serialize())")
        #expect(
            out.result["error"].asString?.contains("disconnected before answering") == true,
            "\(out.result.serialize())")
        try await until("peer dropped") { k.peerRoutes(PAGE).isEmpty }
        let gone = await sendToPage(k, around()).value
        #expect(gone.result["reason"].asString == "unknown_agent", "\(gone.result.serialize())")
        await stop(k)
    }

    @Test(.timeLimit(.minutes(1)))
    func oneGuidPerConnection() async throws {
        let k = try await webKernel()
        let ws = socket(k)
        try await push(ws, hello(PAGE))
        try await until("peer registered") { !k.peerRoutes(PAGE).isEmpty }
        // the same connection cannot become someone else
        try await push(ws, hello("other"))
        let (err, _) = try await next(ws, "error")
        #expect(err["arg"].asString == "guid", "\(err.serialize())")
        #expect(err["error"].asString?.contains(PAGE) == true, "\(err.serialize())")
        // and a malformed guid is refused
        try await push(ws, hello("a|b"))
        let (err3, _) = try await next(ws, "error")
        #expect(err3["arg"].asString == "guid", "\(err3.serialize())")
        ws.cancel(with: .normalClosure, reason: nil)
        await stop(k)
    }

    /// Owner, 2026-10-03: every tab says hello with the identity's guid; the host
    /// calls ALL of them, the first reply settles, the other is dropped; a closed
    /// tab leaves the guid routable through the other.
    @Test(.timeLimit(.minutes(1)))
    func twoTabsOfOneIdentityAreOnePrincipal() async throws {
        let k = try await webKernel()
        let ws = socket(k)
        let ws2 = socket(k)
        try await push(ws, hello(PAGE))
        try await push(ws2, hello(PAGE))
        try await until("both tabs registered") { k.peerRoutes(PAGE).count == 2 }
        let row: JSON = .object(["guid": .string(PAGE), "via": .string("leg")])
        #expect(k.peerRows() == .array([row, row]))
        let send = sendToPage(k, around())
        let (c1, _) = try await next(ws, "call")
        let (c2, _) = try await next(ws2, "call")
        for c in [c1, c2] {
            #expect(c["target"].asString == "canvas", "\(c.serialize())")
            #expect(c["caller"].asString == "\(HOST)|kernel", "\(c.serialize())")
        }
        try await push(
            ws2,
            .object([
                "type": .string("reply"), "id": c2["id"], "data": .object(["tab": .integer(2)]),
            ]))
        let out = await send.value
        #expect(out.result == .object(["tab": .integer(2)]), "\(out.result.serialize())")
        // dropped: nothing waits on it any more
        try await push(
            ws,
            .object([
                "type": .string("reply"), "id": c1["id"], "data": .object(["tab": .integer(1)]),
            ]))
        // one tab closes: the identity stays an address through the other
        ws.cancel(with: .normalClosure, reason: nil)
        try await until("one tab left") { k.peerRoutes(PAGE).count == 1 }
        let again = sendToPage(k, around())
        let (c3, _) = try await next(ws2, "call")
        try await push(
            ws2,
            .object([
                "type": .string("reply"), "id": c3["id"],
                "data": .object(["tab": .integer(2), "n": .integer(2)]),
            ]))
        let out2 = await again.value
        #expect(
            out2.result == .object(["tab": .integer(2), "n": .integer(2)]),
            "\(out2.result.serialize())")
        ws2.cancel(with: .normalClosure, reason: nil)
        try await until("no tab left") { k.peerRoutes(PAGE).isEmpty }
        await stop(k)
    }

    /// MULTI-TAB IDENTITY over the wire: a host emit to `<guid>|<agent>` is an
    /// `emit` frame on EVERY tab of that guid, and the reply one tab's call got is
    /// a `reply_mirror` on the other.
    @Test(.timeLimit(.minutes(1)))
    func everyTabGetsTheEmitAndASiblingsReplyMirrored() async throws {
        let k = try await webKernel()
        let ws = socket(k)
        let ws2 = socket(k)
        try await push(ws, hello(PAGE))
        try await push(ws2, hello(PAGE))
        try await until("both tabs registered") { k.peerRoutes(PAGE).count == 2 }
        await k.emit(AgentId("\(PAGE)|canvas"), .object(["type": .string("tick")]))
        let want: JSON = .object([
            "type": .string("emit"), "target": .string("canvas"),
            "payload": .object(["type": .string("tick")]),
        ])
        #expect(try await next(ws, "emit").0 == want)
        #expect(try await next(ws2, "emit").0 == want)
        try await push(
            ws,
            .object([
                "type": .string("call"), "target": .string("web"), "id": .string("m1"),
                "payload": .object(["type": .string("reflect")]),
            ]))
        let (reply, _) = try await next(ws, "reply")
        let (mirror, _) = try await next(ws2, "reply_mirror")
        #expect(mirror["id"] == .string("m1"), "\(mirror.serialize())")
        #expect(mirror["data"] == reply["data"], "\(mirror.serialize())")
        ws.cancel(with: .normalClosure, reason: nil)
        ws2.cancel(with: .normalClosure, reason: nil)
        try await until("no tab left") { k.peerRoutes(PAGE).isEmpty }
        await stop(k)
    }

    @Test(.timeLimit(.minutes(1)))
    func aSealedLegCarriesNoPeer() async throws {
        let k = try await webKernel(ingress: "deny_ingress")
        let ws = socket(k)
        try await push(ws, hello(PAGE))
        try await Task.sleep(nanoseconds: 200_000_000)
        #expect(k.peerRoutes(PAGE).isEmpty)
        ws.cancel(with: .normalClosure, reason: nil)
        await stop(k)
    }

    @Test(.timeLimit(.minutes(1)))
    func aCallRacingTheDisconnectSettlesInsteadOfHanging() async throws {
        let k = try await webKernel()
        let ws = socket(k)
        try await push(ws, hello(PAGE))
        try await until("peer registered") { !k.peerRoutes(PAGE).isEmpty }
        let route = try #require(k.peerRoutes(PAGE).first)  // resolved BEFORE the close, used after it
        ws.cancel(with: .normalClosure, reason: nil)
        try await until("peer dropped") { k.peerRoutes(PAGE).isEmpty }
        let (out, _) = await route.call(
            "canvas", around(), caller: "\(HOST)|kernel", secret: [], body: nil)
        #expect(out["reason"].asString == "transport_error", "\(out.serialize())")
        await stop(k)
    }

    /// Raw bytes both ways: a body rides out at `payload.bytes` and back at
    /// `data.bytes` — the binary-frame codec every leg shares, never base64.
    @Test(.timeLimit(.minutes(1)))
    func rawBytesRideTheBinaryFrameBothWays() async throws {
        let k = try await webKernel()
        let ws = socket(k)
        try await push(ws, hello(PAGE))
        try await until("peer registered") { !k.peerRoutes(PAGE).isEmpty }
        let send = sendToPage(
            k, .object(["type": .string("write_stream")]),
            binary: Blob("bytes", Data([0, 1, 2, 255])))
        let (call, body) = try await next(ws, "call")
        #expect(call["_binary_path"] == ["payload", "bytes"], "\(call.serialize())")
        #expect(body == Data([0, 1, 2, 255]))
        let head: JSON = .object([
            "type": .string("reply"), "id": call["id"],
            "data": .object(["bytes": .null]), "_binary_path": ["data", "bytes"],
        ])
        try await ws.send(.data(Codec.encodeBinaryFrame(header: head, body: Data([9, 8]))))
        let out = await send.value
        #expect(out.status == .completed, "\(out.result.serialize())")
        #expect(out.body == Blob("bytes", Data([9, 8])))
        ws.cancel(with: .normalClosure, reason: nil)
        await stop(k)
    }

    /// web_rest's `X-Fantastic-Bytes-Field: a.b` binds the LITERAL key `"a.b"`,
    /// and it stays literal on its way to a peer: the call frame names it
    /// `["payload", "a.b"]`, beside a nested `a` → `b` left as sent. The page
    /// answers with its chunk at the LITERAL reply key `x.y` (`["data", "x.y"]`),
    /// and it comes back as the octet body with `X-Fantastic-Bytes-Field: x.y` and
    /// the reply WITHOUT that key on `X-Fantastic-Payload`. Python
    /// `test_web_rest_literal_binding_stays_literal_across_a_bridge` (d).
    @Test(.timeLimit(.minutes(1)))
    func aRestLiteralBindingStaysLiteralToAPeer() async throws {
        let k = try await webKernel()
        let made = await k.send([
            Item(
                AgentId("kernel"),
                .object([
                    "type": .string("create_agent"), "parent_id": .string("web"),
                    "handler_module": .string("web_rest.tools"), "id": .string("rest"),
                    "ingress_rule": .string("allow_all_ingress"),
                    "can_call": .object(["\(PAGE)|canvas": .string("*")]),
                ]))
        ])[0].result
        #expect(made["id"].asString == "rest", "\(made.serialize())")
        _ = await k.send([
            Item(AgentId("web"), .object(["type": .string("mount"), "child_id": .string("rest")]))
        ])[0].result
        let ws = socket(k)
        try await push(ws, hello(PAGE))
        try await until("peer registered") { !k.peerRoutes(PAGE).isEmpty }
        let raw = Data([0x00]) + Data("raw".utf8) + Data([0xFF])
        var req = URLRequest(
            url: URL(string: "http://127.0.0.1:\(k.httpPort())/rest/\(PAGE)%7Ccanvas")!)
        req.httpMethod = "POST"
        req.setValue("application/octet-stream", forHTTPHeaderField: "Content-Type")
        req.setValue(
            #"{"type": "take", "a": {"b": "kept"}}"#, forHTTPHeaderField: "X-Fantastic-Payload")
        req.setValue("a.b", forHTTPHeaderField: "X-Fantastic-Bytes-Field")
        req.httpBody = raw
        let request = req
        let post = Task { () async throws -> (Int, Data, String?, String?) in
            let (d, resp) = try await URLSession(configuration: .ephemeral).data(for: request)
            let http = resp as? HTTPURLResponse
            return (
                http?.statusCode ?? 0, d,
                http?.value(forHTTPHeaderField: "X-Fantastic-Bytes-Field"),
                http?.value(forHTTPHeaderField: "X-Fantastic-Payload")
            )
        }
        let (call, body) = try await next(ws, "call")
        #expect(call["_binary_path"] == ["payload", "a.b"], "\(call.serialize())")
        // the null sits at the literal key; the nested twin is untouched
        #expect(
            call["payload"]
                == .object([
                    "type": .string("take"), "a": .object(["b": .string("kept")]), "a.b": .null,
                ]),
            "\(call.serialize())")
        #expect(call["caller"].asString == "\(HOST)|rest", "\(call.serialize())")
        #expect(body == raw)
        let head: JSON = .object([
            "type": .string("reply"), "id": call["id"],
            "data": .object([
                "keys": .array(["a", "a.b", "type"].map { .string($0) }),
                "x.y": .null, "x": .object(["y": .string("kept")]),
            ]),
            "_binary_path": ["data", "x.y"],
        ])
        try await ws.send(.data(Codec.encodeBinaryFrame(header: head, body: Data("back".utf8))))
        let (status, chunk, field, rest) = try await post.value
        #expect(status == 200, "\(String(decoding: chunk, as: UTF8.self))")
        #expect(chunk == Data("back".utf8))
        #expect(field == "x.y", "the reply's first top-level bytes field")
        #expect(
            try JSON.parse(rest ?? "")
                == .object([
                    "keys": .array(["a", "a.b", "type"].map { .string($0) }),
                    "x": .object(["y": .string("kept")]),
                ]),
            "\(rest ?? "")")
        ws.cancel(with: .normalClosure, reason: nil)
        await stop(k)
    }
}
