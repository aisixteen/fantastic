// 8C: WebSocket upgrade + frame protocol tests.

import FantasticJSON
import FantasticKernel
import FantasticKernelStartup
import Foundation
import Testing

@testable import FantasticWeb

/// A verb that never returns on its own — the in-flight half of the
/// disconnect test. Records that its sleep was CANCELLED (the WS handler
/// aborting its tracked task) rather than completing.
private final class SlowFlag: @unchecked Sendable {
    var cancelled = false
}

private struct SlowProbe: AgentBundle {
    let name = "ws_slow"
    let flag: SlowFlag
    func handle(agentId: AgentId, payload: JSON, kernel: Kernel) async throws -> JSON? {
        guard payload["type"].asString == "work" else { return .object(["ok": .bool(true)]) }
        do {
            try await Task.sleep(nanoseconds: 30_000_000_000)
        } catch {
            flag.cancelled = true
            throw error
        }
        return .object(["done": .bool(true)])
    }
}

@Suite("WebSocket", .serialized)
struct WebSocketTests {
    /// WS is opt-in (parity with Python): the host serves `/<id>/ws`
    /// only when a `web_ws` child contributes the route. `web` already
    /// auto-booted via `startKernelInMemory`, so create the child then
    /// hot-`mount` it onto the live server.
    ///
    /// `granted` is the leg's EGRESS half. An inbound frame dispatches AS THE LEG
    /// (see `WebSocketProxyHandler.trackCall`), so reaching `core` — the leg's
    /// GRANDparent, which no structural baseline token covers — needs an explicit
    /// grant on BOTH ends. `granted: false` leaves the leg open inbound and
    /// granted nothing outbound: the sealed-by-composition case.
    @discardableResult
    private func mountWebWS(_ kernel: Kernel, granted: Bool = true) async -> String {
        var payload: JSON = .object([
            "type": .string("create_agent"),
            "parent_id": .string("web"),
            "handler_module": .string("web_ws.tools"),
            "id": .string("web_ws"),
            // The web_ws leg SEALS by default — open it for the inbound call.
            "ingress_rule": .string("allow_all_ingress"),
        ])
        if granted {
            payload["can_call"] = .object(["core": .string("*")])
        }
        let rec = await kernel.send([Item(AgentId("kernel"), payload)])[0].result
        let wsId = rec["id"].asString ?? "web_ws"
        if granted {
            // The INGRESS half, on the root: the leg is core's GRANDchild, so
            // `$children` does not reach it.
            var grants: JSON = .object([
                "$self": .string("*"), "$parent": .string("*"), "$children": .string("*"),
            ])
            grants[wsId] = .string("*")
            _ = await kernel.send([Item(AgentId("core"), .object([
                    "type": .string("update_agent"),
                    "id": .string("core"),
                    "callable_by": grants,
                ]))])[0].result
        }
        _ = await kernel.send([Item(AgentId("web"), .object(["type": .string("mount"), "child_id": .string(wsId)]))])[0].result
        return wsId
    }

    // NOTE: the former frame-unit tests (computeWebSocketAccept / decodeFrame)
    // are gone — swift-nio's WebSocket upgrader + frame codec own the handshake
    // hash and framing now. The two end-to-end tests below exercise that real
    // path through the NIO server (a stronger guarantee than the hand-rolled
    // codec the unit tests used to pin).

    @Test func wsCallRoundTripsThroughServer() async throws {
        let kernel = try await startWebKernel()
        _ = await kernel.send([Item(AgentId("web"), .object(["type": .string("boot")]))])[0].result
        await mountWebWS(kernel)
        defer {
            Task {
                _ = await kernel.send([Item(AgentId("web"), .object(["type": .string("stop")]))])[0].result
            }
        }
        let port = kernel.httpPort()
        let url = URL(string: "ws://127.0.0.1:\(port)/core/ws")!
        let task = URLSession.shared.webSocketTask(with: url)
        task.resume()

        // Send a call frame asking for list_agents on core.
        let frame: JSON = .object([
            "type": .string("call"),
            "id": .string("c1"),
            "target": .string("core"),
            "payload": .object(["type": .string("list_agents")]),
        ])
        try await task.send(.string(frame.serialize()))

        // Read until the reply arrives. The connection auto-watches
        // `core`, so the call's own inbox fanout can surface an
        // `event` frame before the `reply` — a real client (browser
        // transport.js, the Swift bridge) correlates replies by `id`
        // and handles events separately, so filter by frame type.
        var reply: JSON? = nil
        for _ in 0..<5 {
            let msg = try await task.receive()
            let text: String
            switch msg {
            case .string(let s): text = s
            case .data(let d): text = String(data: d, encoding: .utf8) ?? ""
            @unknown default: continue
            }
            guard let parsed = try? JSON.parse(text) else { continue }
            if parsed["type"].asString == "reply", parsed["id"].asString == "c1" {
                reply = parsed
                break
            }
        }
        let parsed = try #require(reply)
        let agents = parsed["data"]["agents"].asArray ?? []
        let ids = agents.compactMap { $0["id"].asString }
        #expect(ids.contains("core"))

        task.cancel(with: .normalClosure, reason: nil)
    }

    @Test func wsExplicitWatchStreamsEvents() async throws {
        // Parity with Python's `_proxy._on_watch`: connect to one
        // agent's WS endpoint, then send `{type:"watch", src:<other>}`
        // and verify emits on <other> arrive as `event` frames. This
        // is the path kernel_bridge.watch_remote drives.
        let kernel = try await startWebKernel()
        _ = await kernel.send([Item(AgentId("web"), .object(["type": .string("boot")]))])[0].result
        await mountWebWS(kernel)
        defer {
            Task {
                _ = await kernel.send([Item(AgentId("web"), .object(["type": .string("stop")]))])[0].result
            }
        }
        let port = kernel.httpPort()
        // Connect on /core/ws but explicitly watch a DIFFERENT source
        // so we exercise the explicit-watch path, not the auto-watch.
        let url = URL(string: "ws://127.0.0.1:\(port)/core/ws")!
        let task = URLSession.shared.webSocketTask(with: url)
        task.resume()

        // Subscribe to `web`'s inbox explicitly.
        let watchFrame: JSON = .object([
            "type": .string("watch"),
            "src": .string("web"),
        ])
        try await task.send(.string(watchFrame.serialize()))

        // Give the server a tick to register the watch, then emit on
        // `web`.
        try await Task.sleep(nanoseconds: 100_000_000)
        await kernel.emit(
            AgentId("web"),
            .object(["type": .string("token"), "text": .string("hi")]))

        // Read frames until we see the token event (skip any other
        // event frames the auto-watch on core might surface).
        var sawToken = false
        for _ in 0..<5 {
            let msg = try await task.receive()
            let text: String
            switch msg {
            case .string(let s): text = s
            case .data(let d): text = String(data: d, encoding: .utf8) ?? ""
            @unknown default: continue
            }
            guard let parsed = try? JSON.parse(text) else { continue }
            if parsed["type"].asString == "event",
                parsed["payload"]["type"].asString == "token"
            {
                #expect(parsed["payload"]["text"].asString == "hi")
                sawToken = true
                break
            }
        }
        #expect(sawToken, "explicit watch did not stream the token event")

        task.cancel(with: .normalClosure, reason: nil)
    }
    // MARK: - identity + lifetime of an inbound call

    @Test func wsCallIsGatedAsTheLegNotTheOperator() async throws {
        // `handleText` spawned a bare `Task {}`, which does NOT inherit
        // `KernelTaskLocals.currentSender` — so `BatchSend.launch` saw nil,
        // read the frame as an OPERATOR call, and skipped the two-sided gate
        // entirely. Every browser call was ungated, bounded only by the leg's
        // ingress rule. Here the leg is OPEN inbound and granted NOTHING
        // outbound: an operator call would list every agent; a leg call refuses.
        let kernel = try await startWebKernel()
        _ = await kernel.send([Item(AgentId("web"), .object(["type": .string("boot")]))])[0].result
        await mountWebWS(kernel, granted: false)
        defer {
            Task {
                _ = await kernel.send([Item(AgentId("web"), .object(["type": .string("stop")]))])[0].result
            }
        }
        let port = kernel.httpPort()
        let url = URL(string: "ws://127.0.0.1:\(port)/core/ws")!
        let task = URLSession.shared.webSocketTask(with: url)
        task.resume()
        let frame: JSON = .object([
            "type": .string("call"),
            "id": .string("g1"),
            "target": .string("core"),
            "payload": .object(["type": .string("list_agents")]),
        ])
        try await task.send(.string(frame.serialize()))

        var reply: JSON? = nil
        for _ in 0..<5 {
            let msg = try await task.receive()
            let text: String
            switch msg {
            case .string(let str): text = str
            case .data(let d): text = String(data: d, encoding: .utf8) ?? ""
            @unknown default: continue
            }
            guard let parsed = try? JSON.parse(text) else { continue }
            if parsed["type"].asString == "reply", parsed["id"].asString == "g1" {
                reply = parsed
                break
            }
        }
        let parsed = try #require(reply)
        #expect(parsed["data"]["reason"].asString == "unauthorized", "\(parsed)")
        #expect(parsed["data"]["agents"].asArray == nil, "the tree leaked past the gate")
        task.cancel(with: .normalClosure, reason: nil)
    }

    @Test func wsDisconnectCancelsInFlightCalls() async throws {
        // A `call` can run for minutes (an LLM stream). Untracked, a client that
        // closed its tab left it running to completion — billing tokens — with
        // nowhere to write the reply. Python cancels its pending set on
        // disconnect and rust aborts its JoinHandles; `teardown()` now does too.
        let kernel = try await startWebKernel()
        let flag = SlowFlag()
        kernel.bundles.register("ws_slow.tools", SlowProbe(flag: flag))
        _ = await kernel.send([Item(AgentId("web"), .object(["type": .string("boot")]))])[0].result
        let wsId = await mountWebWS(kernel)
        // A CHILD of the leg: the structural baseline ($parent/$children) covers
        // both directions, so the gate is not what this test is measuring.
        let probeId = await kernel.send([Item(AgentId("kernel"), .object([
                "type": .string("create_agent"),
                "parent_id": .string(wsId),
                "handler_module": .string("ws_slow.tools"),
                "id": .string("ws_slow_probe"),
            ]))])[0].result["id"].asString ?? "ws_slow_probe"
        defer {
            Task {
                _ = await kernel.send([Item(AgentId("web"), .object(["type": .string("stop")]))])[0].result
            }
        }
        let port = kernel.httpPort()
        let url = URL(string: "ws://127.0.0.1:\(port)/core/ws")!
        let task = URLSession.shared.webSocketTask(with: url)
        task.resume()
        try await task.send(.string(JSON.object([
            "type": .string("call"),
            "id": .string("slow1"),
            "target": .string(probeId),
            "payload": .object(["type": .string("work")]),
        ]).serialize()))

        // The dispatch is a kernel TASK — wait until `ps` sees it.
        func slowTaskLive() async -> Bool {
            let ps = await kernel.send([Item(AgentId("kernel"), .object(["type": .string("ps")]))])[0].result
            return (ps["tasks"].asArray ?? []).contains { $0["agent"].asString == probeId }
        }
        var started = false
        for _ in 0..<40 where !started {
            if await slowTaskLive() { started = true; break }
            try await Task.sleep(nanoseconds: 50_000_000)
        }
        #expect(started, "the slow call never reached the task table")

        task.cancel(with: .abnormalClosure, reason: nil)

        var gone = false
        for _ in 0..<60 where !gone {
            try await Task.sleep(nanoseconds: 50_000_000)
            gone = await !slowTaskLive()
        }
        #expect(gone, "disconnect left the dispatch running")
        #expect(flag.cancelled, "the handler was never cancelled, it was abandoned")
    }
    /// Shared teardown for the tests below.
    private func stopWebHost(_ kernel: Kernel) async {
        _ = await kernel.send([Item(AgentId("web"), .object(["type": .string("stop")]))])[0].result
    }

    // MARK: - A13.14 / A13.16 / A13.17 / A13.18

    /// A13.14 — every server→client text frame is python's `encode_frame`:
    /// `json.dumps(envelope, default=str)`, i.e. `', '`/`': '` separators and
    /// `ensure_ascii=True`. `serialize()` sent compact raw UTF-8, so `·` and
    /// `↑` crossed the wire unescaped.
    @Test func eventFrameBytesMatchPythonJsonDumps() async throws {
        let kernel = try await startWebKernel()
        _ = await kernel.send([Item(AgentId("web"), .object(["type": .string("boot")]))])[0].result
        await mountWebWS(kernel)
        defer { Task { await stopWebHost(kernel) } }
        let port = kernel.httpPort()
        let task = URLSession(configuration: .ephemeral)
            .webSocketTask(with: URL(string: "ws://127.0.0.1:\(port)/core/ws")!)
        task.resume()
        defer { task.cancel(with: .normalClosure, reason: nil) }
        // The connection auto-watches `core`; emit something with non-ASCII.
        try await Task.sleep(nanoseconds: 100_000_000)
        await kernel.emit(
            AgentId("core"), .object(["text": .string("[reply to 128 · files reflect · ↑126]")]))
        var frame = ""
        for _ in 0..<5 {
            if case .string(let s) = try await task.receive(), s.contains("\"event\"") {
                frame = s
                break
            }
        }
        #expect(frame.hasPrefix("{\"type\": \"event\", \"payload\": {"), "\(frame)")
        #expect(frame.contains("\\u00b7"), "\(frame)")
        #expect(frame.contains("\\u2191"), "\(frame)")
        #expect(!frame.contains("·"), "\(frame)")
    }

    /// A13.16 — a client Close is echoed WITH ITS PAYLOAD; swift echoed an
    /// empty buffer, which a client reads as 1005 (no status received).
    @Test func clientCloseIsEchoedWithItsCode() async throws {
        let kernel = try await startWebKernel()
        _ = await kernel.send([Item(AgentId("web"), .object(["type": .string("boot")]))])[0].result
        await mountWebWS(kernel)
        defer { Task { await stopWebHost(kernel) } }
        let port = kernel.httpPort()
        let recorder = CloseRecorder()
        let session = URLSession(
            configuration: .ephemeral, delegate: recorder, delegateQueue: nil)
        let task = session.webSocketTask(with: URL(string: "ws://127.0.0.1:\(port)/core/ws")!)
        task.resume()
        try await Task.sleep(nanoseconds: 100_000_000)
        task.cancel(with: .normalClosure, reason: nil)
        var code: URLSessionWebSocketTask.CloseCode?
        for _ in 0..<40 where code == nil {
            try await Task.sleep(nanoseconds: 50_000_000)
            code = recorder.code()
        }
        #expect(code == .normalClosure, "\(String(describing: code))")
    }

    /// A13.17 (D16) — uvicorn's 101 also carries `date` and `server: uvicorn`.
    /// permessage-deflate needs a compression implementation and is recorded,
    /// not ported.
    @Test func wsUpgradeCarriesDateAndServerHeaders() async throws {
        let kernel = try await startWebKernel()
        _ = await kernel.send([Item(AgentId("web"), .object(["type": .string("boot")]))])[0].result
        await mountWebWS(kernel)
        defer { Task { await stopWebHost(kernel) } }
        let port = kernel.httpPort()
        let sock = try RawHTTPClient(port: port)
        defer { sock.close() }
        try sock.write(
            "GET /core/ws HTTP/1.1\r\nHost: h\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n"
                + "Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n"
                + "Sec-WebSocket-Version: 13\r\n\r\n")
        let raw = sock.readSome()
        #expect(raw.hasPrefix("HTTP/1.1 101 "), "\(raw)")
        #expect(raw.lowercased().contains("sec-websocket-accept:"), "\(raw)")
        #expect(raw.contains("date: "), "\(raw)")
        #expect(raw.contains("server: uvicorn"), "\(raw)")
    }

    /// A13.18 (D16) — the server PINGS (uvicorn's `ws_ping_interval`) and closes
    /// 1011 when no pong comes back inside `ws_ping_timeout`. Both are 20 s, so
    /// the behaviour is source-only; the wiring is pinned here.
    @Test func serverPingsAndCloses1011WithoutPong() {
        #expect(WebSocketProxyHandler.pingIntervalSeconds == 20)
        #expect(WebSocketProxyHandler.pongTimeoutSeconds == 20)
    }
}

/// Captures the close code the SERVER answered with.
final class CloseRecorder: NSObject, URLSessionWebSocketDelegate, @unchecked Sendable {
    private let lock = NSLock()
    private var closeCode: URLSessionWebSocketTask.CloseCode?

    func urlSession(
        _ session: URLSession, webSocketTask: URLSessionWebSocketTask,
        didCloseWith closeCode: URLSessionWebSocketTask.CloseCode, reason: Data?
    ) {
        lock.withLock { self.closeCode = closeCode }
    }

    func code() -> URLSessionWebSocketTask.CloseCode? {
        lock.lock()
        defer { lock.unlock() }
        return closeCode
    }
}
