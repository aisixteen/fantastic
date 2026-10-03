// The ONE watch path (connect-time auto-watch + explicit `watch` frame): the
// name resolves the way `send` resolves a target (an id or `kernel`), the watch
// runs under the leg's own identity, and a refusal is an `error` frame — never
// silence, and never recorded as watched. Measured before (python, same shape
// here): a socket whose connect-time watch did not resolve mirrored nothing and
// said nothing, and the explicit watch that followed was skipped as a duplicate.
//
// Twin of python `bundled_agents/io/web_ws/tests/test_watch_path.py`.

import FantasticIoBridge
import FantasticJSON
import FantasticKernel
import FantasticKernelStartup
import Foundation
import OrderedCollections
import Testing

@testable import FantasticWeb

private let BASE: OrderedDictionary<String, JSON> = [
    "$self": .string("*"), "$parent": .string("*"), "$children": .string("*"),
]

private func create(_ k: Kernel, _ rec: OrderedDictionary<String, JSON>) async {
    var p: OrderedDictionary<String, JSON> = ["type": .string("create_agent")]
    for (kk, v) in rec { p[kk] = v }
    let out = await k.send([Item(AgentId("kernel"), .object(p))])[0].result
    #expect(out["id"] == rec["id"], "\(out.serialize())")
}

/// A booted web host with the `leg` web_ws leg mounted on it.
private func webKernel() async throws -> Kernel {
    let k = try await startWebKernel()
    _ = await k.send([Item(AgentId("web"), .object(["type": .string("boot")]))])[0].result
    return k
}

private func mountLeg(_ k: Kernel, grant: Bool) async {
    var canCall = BASE
    if grant { canCall["exec"] = .string("*") }
    await create(
        k,
        [
            "parent_id": .string("web"), "handler_module": .string("web_ws.tools"),
            "id": .string("leg"), "ingress_rule": .string("allow_all_ingress"),
            "can_call": .object(canCall),
        ])
    wire(k, "web", "leg")
    _ = await k.send([
        Item(AgentId("web"), .object(["type": .string("mount"), "child_id": .string("leg")]))
    ])[0].result
}

/// python `_exec`: the leg, and an exec_bridge `exec`.
private func mountExec(_ k: Kernel, grant: Bool) async {
    await mountLeg(k, grant: grant)
    var callableBy = BASE
    if grant { callableBy["leg"] = .string("*") }
    await create(
        k,
        [
            "parent_id": .string("kernel_state"), "handler_module": .string("exec_bridge.tools"),
            "id": .string("exec"),
            "ingress_rule": .string("allow_all_ingress"),
            "allowed": .array([.string("/bin/echo .*")]),
            "cwd": .string("."),
            "callable_by": .object(callableBy),
        ])
}

private func stop(_ k: Kernel) async {
    _ = await k.send([Item(AgentId("web"), .object(["type": .string("stop")]))])[0].result
}

private func socket(_ k: Kernel, _ host: String) -> URLSessionWebSocketTask {
    let task = URLSession(configuration: .ephemeral)
        .webSocketTask(with: URL(string: "ws://127.0.0.1:\(k.httpPort())/\(host)/ws")!)
    task.resume()
    return task
}

/// One frame, BOUNDED: `URLSessionWebSocketTask.receive` ignores task
/// cancellation, so a frame that never comes (the silence this suite is about)
/// would hang the run. Past `seconds` the socket is cancelled, which makes the
/// pending receive throw — a missing frame is a failure, not a stall.
private func next(_ task: URLSessionWebSocketTask, seconds: UInt64 = 10) async throws -> JSON {
    let timer = Task {
        try await Task.sleep(nanoseconds: seconds * 1_000_000_000)
        task.cancel(with: .goingAway, reason: nil)
    }
    defer { timer.cancel() }
    switch try await task.receive() {
    case .string(let s): return try JSON.parse(s)
    case .data(let d): return try Codec.decodeBinaryFrame(d).0
    @unknown default: return .null
    }
}

private func send(_ task: URLSessionWebSocketTask, _ frame: JSON) async throws {
    try await task.send(.string(frame.serialize()))
}

/// python `_until`: every frame up to and including the reply to `callId`.
private func until(_ task: URLSessionWebSocketTask, _ callId: String, n: Int = 40) async throws
    -> [JSON]
{
    var out: [JSON] = []
    for _ in 0..<n {
        let m = try await next(task)
        out.append(m)
        if m["type"].asString == "reply", m["id"].asString == callId { return out }
    }
    Issue.record("no reply \(callId): \(out.map { $0.serialize() })")
    return out
}

private func call(_ id: String, _ payload: JSON) -> JSON {
    .object([
        "type": .string("call"), "target": .string("exec"), "id": .string(id),
        "payload": payload,
    ])
}

@Suite("web_ws — the one watch path", .serialized)
struct WebSocketWatchPathTests {
    @Test(.timeLimit(.minutes(1)))
    func aSocketOpenedOnAnAgentReceivesItsEvents() async throws {
        let k = try await webKernel()
        await mountExec(k, grant: true)
        let ws = socket(k, "exec")
        try await send(
            ws,
            call(
                "1",
                .object([
                    "type": .string("start"), "command": .string("/bin/echo"),
                    "args": .array([.string("v-42")]),
                ])))
        var frames = try await until(ws, "1")
        try await Task.sleep(nanoseconds: 1_000_000_000)
        try await send(ws, call("2", .object(["type": .string("status")])))
        frames += try await until(ws, "2")
        ws.cancel(with: .normalClosure, reason: nil)
        await stop(k)
        let kinds = frames.filter { $0["type"].asString == "event" }.compactMap {
            $0["payload"]["type"].asString
        }
        #expect(
            kinds.contains("job_started") && kinds.contains("job_done"),
            "\(frames.map { $0.serialize() })")
        #expect(
            !frames.contains { $0["type"].asString == "error" },
            "\(frames.map { $0.serialize() })")
    }

    @Test(.timeLimit(.minutes(1)))
    func aRefusedAutoWatchIsAnErrorFrameNotSilence() async throws {
        let k = try await webKernel()
        await mountExec(k, grant: false)
        let ws = socket(k, "exec")
        let first = try await next(ws)
        #expect(
            first["type"].asString == "error" && first["reason"].asString == "unauthorized",
            "\(first.serialize())")
        #expect(
            k.agent(AgentId("exec"))?.watcherIds().isEmpty == true, "not recorded")
        // …and an explicit watch is re-tried, not skipped as a duplicate
        try await send(
            ws, .object(["type": .string("watch"), "src": .string("exec"), "id": .string("w")]))
        let again = try await next(ws)
        #expect(
            again["type"].asString == "error" && again["id"].asString == "w",
            "\(again.serialize())")
        ws.cancel(with: .normalClosure, reason: nil)
        await stop(k)
    }

    @Test(.timeLimit(.minutes(1)))
    func anUnknownNameIsAnErrorFrame() async throws {
        let k = try await webKernel()
        await mountLeg(k, grant: false)
        let ws = socket(k, "nobody")
        let first = try await next(ws)
        ws.cancel(with: .normalClosure, reason: nil)
        await stop(k)
        #expect(
            first["type"].asString == "error" && first["reason"].asString == "unknown_agent",
            "\(first.serialize())")
        #expect(first["src"].asString == "nobody", "\(first.serialize())")
    }
}
