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

private func sendToPage(_ k: Kernel, _ payload: JSON, binary: Data? = nil) -> Task<Outcome, Never>
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
        try await until("peer registered") { k.peer(PAGE) != nil }
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
        try await until("peer registered") { k.peer(PAGE) != nil }
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
        try await until("peer registered") { k.peer(PAGE) != nil }
        let send = sendToPage(k, around())
        _ = try await next(ws, "call")
        ws.cancel(with: .normalClosure, reason: nil)
        let out = await send.value
        #expect(out.result["reason"].asString == "transport_error", "\(out.result.serialize())")
        #expect(
            out.result["error"].asString?.contains("disconnected before answering") == true,
            "\(out.result.serialize())")
        try await until("peer dropped") { k.peer(PAGE) == nil }
        let gone = await sendToPage(k, around()).value
        #expect(gone.result["reason"].asString == "unknown_agent", "\(gone.result.serialize())")
        await stop(k)
    }

    @Test(.timeLimit(.minutes(1)))
    func oneGuidPerConnectionAndPerKernel() async throws {
        let k = try await webKernel()
        let ws = socket(k)
        let ws2 = socket(k)
        try await push(ws, hello(PAGE))
        try await until("peer registered") { k.peer(PAGE) != nil }
        // the same connection cannot become someone else
        try await push(ws, hello("other"))
        let (err, _) = try await next(ws, "error")
        #expect(err["arg"].asString == "guid", "\(err.serialize())")
        #expect(err["error"].asString?.contains(PAGE) == true, "\(err.serialize())")
        // another connection cannot take a guid that is live
        try await push(ws2, hello(PAGE))
        let (err2, _) = try await next(ws2, "error")
        #expect(
            err2["error"].asString?.contains("already connected via leg") == true,
            "\(err2.serialize())")
        // and a malformed guid is refused
        try await push(ws2, hello("a|b"))
        let (err3, _) = try await next(ws2, "error")
        #expect(err3["arg"].asString == "guid", "\(err3.serialize())")
        ws.cancel(with: .normalClosure, reason: nil)
        ws2.cancel(with: .normalClosure, reason: nil)
        await stop(k)
    }

    @Test(.timeLimit(.minutes(1)))
    func aSealedLegCarriesNoPeer() async throws {
        let k = try await webKernel(ingress: "deny_ingress")
        let ws = socket(k)
        try await push(ws, hello(PAGE))
        try await Task.sleep(nanoseconds: 200_000_000)
        #expect(k.peer(PAGE) == nil)
        ws.cancel(with: .normalClosure, reason: nil)
        await stop(k)
    }

    @Test(.timeLimit(.minutes(1)))
    func aCallRacingTheDisconnectSettlesInsteadOfHanging() async throws {
        let k = try await webKernel()
        let ws = socket(k)
        try await push(ws, hello(PAGE))
        try await until("peer registered") { k.peer(PAGE) != nil }
        let route = try #require(k.peer(PAGE))  // resolved BEFORE the close, used after it
        ws.cancel(with: .normalClosure, reason: nil)
        try await until("peer dropped") { k.peer(PAGE) == nil }
        let (out, _) = await route.call(
            "canvas", around(), caller: "\(HOST)|kernel", body: nil)
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
        try await until("peer registered") { k.peer(PAGE) != nil }
        let send = sendToPage(
            k, .object(["type": .string("write_stream")]), binary: Data([0, 1, 2, 255]))
        let (call, body) = try await next(ws, "call")
        #expect(call["_binary_path"].asString == "payload.bytes", "\(call.serialize())")
        #expect(body == Data([0, 1, 2, 255]))
        let head: JSON = .object([
            "type": .string("reply"), "id": call["id"],
            "data": .object(["bytes": .null]), "_binary_path": .string("data.bytes"),
        ])
        try await ws.send(.data(Codec.encodeBinaryFrame(header: head, body: Data([9, 8]))))
        let out = await send.value
        #expect(out.status == .completed, "\(out.result.serialize())")
        #expect(out.body == Data([9, 8]))
        ws.cancel(with: .normalClosure, reason: nil)
        await stop(k)
    }
}
