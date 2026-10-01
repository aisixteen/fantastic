// web_ws peer calls — the CANCEL half of the per-connection correlator.
//
// `PeerCallsTests` pins the round trip, the page-side refusal and the
// disconnect settle. What it leaves open is the caller giving up: a call
// cancelled while it waits settles `{cancelled:true}` and a late answer for it
// is dropped; a call cancelled BEFORE it parks settles the same way without a
// frame ever reaching the page. Frame ids are `h<n>` per connection, and a
// minted-but-abandoned id is not reused. A shared correlator must keep all of it.

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

private func webKernel() async throws -> Kernel {
    let k = try await startWebKernel()
    _ = await k.send([Item(AgentId("web"), .object(["type": .string("boot")]))])[0].result
    k.root?.updateMeta(["guid": .string(HOST)])
    _ = await k.send([
        Item(
            AgentId("kernel"),
            .object([
                "type": .string("create_agent"), "parent_id": .string("web"),
                "handler_module": .string("web_ws.tools"), "id": .string("leg"),
                "ingress_rule": .string("allow_all_ingress"),
            ]))
    ])[0].result
    _ = await k.send([
        Item(AgentId("web"), .object(["type": .string("mount"), "child_id": .string("leg")]))
    ])[0].result
    return k
}

private func socket(_ k: Kernel) -> URLSessionWebSocketTask {
    let task = URLSession(configuration: .ephemeral)
        .webSocketTask(with: URL(string: "ws://127.0.0.1:\(k.httpPort())/web/ws")!)
    task.resume()
    return task
}

/// The next text frame of type `ftype`, bounded (a missing frame fails).
private func next(_ task: URLSessionWebSocketTask, _ ftype: String) async throws -> JSON {
    let timer = Task {
        try await Task.sleep(nanoseconds: 5_000_000_000)
        task.cancel(with: .goingAway, reason: nil)
    }
    defer { timer.cancel() }
    while true {
        guard case .string(let s) = try await task.receive() else { continue }
        let frame = try JSON.parse(s)
        if frame["type"].asString == ftype { return frame }
    }
}

private func push(_ task: URLSessionWebSocketTask, _ frame: JSON) async throws {
    try await task.send(.string(frame.serialize()))
}

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

private let around: JSON = .object(["type": .string("around")])

@Suite("web_ws — a peer call the caller gives up", .serialized)
struct PeerCallCancelPinTests {

    @Test(.timeLimit(.minutes(1)))
    func aCancelledCallSettlesCancelledAndItsLateAnswerIsDropped() async throws {
        let k = try await webKernel()
        let ws = socket(k)
        try await push(ws, .object(["type": .string("hello"), "guid": .string(PAGE)]))
        try await until("peer registered") { k.peer(PAGE) != nil }
        let route = try #require(k.peer(PAGE))

        // cancelled WHILE waiting on the page
        let waiting = Task {
            await route.call("canvas", around, caller: "\(HOST)|kernel", body: nil)
        }
        let first = try await next(ws, "call")
        #expect(first["id"].asString == "h1", "\(first.serialize())")
        waiting.cancel()
        let (out, body) = await waiting.value
        #expect(out.serialize() == #"{"cancelled":true}"#)
        #expect(body == nil)
        // its late answer finds no waiter and changes nothing
        try await push(
            ws,
            .object([
                "type": .string("reply"), "id": .string("h1"),
                "data": .object(["late": .bool(true)]),
            ]))

        // cancelled BEFORE it parks: settles at once, its frame never goes out
        let early = Task { () -> (JSON, Data?) in
            while !Task.isCancelled { await Task.yield() }
            return await route.call("canvas", around, caller: "\(HOST)|kernel", body: nil)
        }
        early.cancel()
        #expect((await early.value).0.serialize() == #"{"cancelled":true}"#)

        // the connection still serves calls; the abandoned id was not reused
        let live = Task { await route.call("canvas", around, caller: "\(HOST)|kernel", body: nil) }
        let third = try await next(ws, "call")
        #expect(third["id"].asString == "h3", "\(third.serialize())")
        try await push(
            ws, .object(["type": .string("reply"), "id": .string("h3"), "data": .string("ok")]))
        // a non-object answer is wrapped as {result}
        #expect((await live.value).0.serialize() == #"{"result":"ok"}"#)

        ws.cancel(with: .normalClosure, reason: nil)
        _ = await k.send([Item(AgentId("web"), .object(["type": .string("stop")]))])[0].result
    }
}
