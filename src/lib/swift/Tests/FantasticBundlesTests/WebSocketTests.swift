// 8C: WebSocket upgrade + frame protocol tests.

import FantasticIoBridge
import FantasticJSON
import FantasticKernel
import FantasticKernelStartup
import Foundation
import Testing

@testable import FantasticWeb
@testable import FantasticWebWS

/// An unreadable frame kind — the one thing `receiveJSON` cannot answer.
private enum WSFrameError: Error { case unknownKind }

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
            payload["can_call"] = .object(["kernel_state": .string("*")])
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
            _ = await kernel.send([Item(AgentId("kernel_state"), .object([
                    "type": .string("update_agent"),
                    "id": .string("kernel_state"),
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

    /// One frame, parsed. A binary frame is read as its `[4B len | head | body]`
    /// header so a caller can assert on the envelope either way.
    static func receiveJSON(_ task: URLSessionWebSocketTask) async throws -> JSON {
        switch try await task.receive() {
        case .string(let s): return try JSON.parse(s)
        case .data(let d): return try Codec.decodeBinaryFrame(d).0
        @unknown default: throw WSFrameError.unknownKind
        }
    }

    /// THE WATCH EVENT PRECEDES THE REPLY, EVERY TIME (python's order).
    ///
    /// Python's ws face is ordered by construction: one asyncio loop, and the
    /// kernel's fanout queues a watcher's payload BEFORE the target's handler
    /// runs. Swift writes the reply from the `ws_call` task and events from the
    /// `ws_drain` task, so the reply could overtake the event the kernel had
    /// already queued — measured as an `event`/`reply` inversion this suite used
    /// to work around by filtering frame types. 50 calls on ONE socket: each
    /// must read back `event` then `reply`, never the other way.
    @Test func watchEventAlwaysPrecedesTheReply() async throws {
        let kernel = try await startWebKernel()
        _ = await kernel.send([Item(AgentId("web"), .object(["type": .string("boot")]))])[0].result
        await mountWebWS(kernel)
        defer {
            Task {
                _ = await kernel.send([Item(AgentId("web"), .object(["type": .string("stop")]))])[0].result
            }
        }
        let port = kernel.httpPort()
        let url = URL(string: "ws://127.0.0.1:\(port)/kernel_state/ws")!
        let task = URLSession.shared.webSocketTask(with: url)
        task.resume()
        defer { task.cancel(with: .normalClosure, reason: nil) }

        for i in 0..<50 {
            let id = "ord\(i)"
            let frame: JSON = .object([
                "type": .string("call"),
                "id": .string(id),
                "target": .string("kernel_state"),
                "payload": .object(["type": .string("list_agents")]),
            ])
            try await task.send(.string(frame.serialize()))
            // Read to THIS call's reply. Every frame before it must be an
            // event, and at least one must have arrived — the fanout of the
            // very call being answered.
            var events = 0
            while true {
                let got = try await Self.receiveJSON(task)
                if got["type"].asString == "event" {
                    events += 1
                    continue
                }
                #expect(got["type"].asString == "reply", "frame \(i): \(got.serialize())")
                #expect(got["id"].asString == id)
                break
            }
            #expect(events >= 1, "call \(i): reply arrived with no event before it")
        }
    }

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
        let url = URL(string: "ws://127.0.0.1:\(port)/kernel_state/ws")!
        let task = URLSession.shared.webSocketTask(with: url)
        task.resume()

        // Send a call frame asking for list_agents on core.
        let frame: JSON = .object([
            "type": .string("call"),
            "id": .string("c1"),
            "target": .string("kernel_state"),
            "payload": .object(["type": .string("list_agents")]),
        ])
        try await task.send(.string(frame.serialize()))

        // The connection auto-watches `kernel_state`, so this call's own
        // fanout queues an `event` for this client BEFORE the handler runs —
        // and python puts that event on the wire BEFORE the reply. Swift does
        // too now (the flush-then-reply step in `WebSocketProxyHandler`), so
        // the FIRST frame is the event and the SECOND is the reply.
        let first = try await Self.receiveJSON(task)
        #expect(first["type"].asString == "event")
        let parsed = try await Self.receiveJSON(task)
        #expect(parsed["type"].asString == "reply")
        #expect(parsed["id"].asString == "c1")
        let agents = parsed["data"]["agents"].asArray ?? []
        let ids = agents.compactMap { $0["id"].asString }
        #expect(ids.contains("kernel_state"))

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
        // Connect on /kernel_state/ws but explicitly watch a DIFFERENT source
        // so we exercise the explicit-watch path, not the auto-watch.
        let url = URL(string: "ws://127.0.0.1:\(port)/kernel_state/ws")!
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
        let url = URL(string: "ws://127.0.0.1:\(port)/kernel_state/ws")!
        let task = URLSession.shared.webSocketTask(with: url)
        task.resume()
        let frame: JSON = .object([
            "type": .string("call"),
            "id": .string("g1"),
            "target": .string("kernel_state"),
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
        let url = URL(string: "ws://127.0.0.1:\(port)/kernel_state/ws")!
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
            .webSocketTask(with: URL(string: "ws://127.0.0.1:\(port)/kernel_state/ws")!)
        task.resume()
        defer { task.cancel(with: .normalClosure, reason: nil) }
        // The connection auto-watches `core`; emit something with non-ASCII.
        try await Task.sleep(nanoseconds: 100_000_000)
        await kernel.emit(
            AgentId("kernel_state"), .object(["text": .string("[reply to 128 · files reflect · ↑126]")]))
        var frame = ""
        for _ in 0..<5 {
            if case .string(let s) = try await task.receive(), s.contains("\"event\"") {
                frame = s
                break
            }
        }
        // The auto-watch's own reader stamps the `src` the event came from.
        #expect(
            frame.hasPrefix("{\"type\": \"event\", \"src\": \"kernel_state\", \"payload\": {"),
            "\(frame)")
        #expect(frame.contains("\\u00b7"), "\(frame)")
        #expect(frame.contains("\\u2191"), "\(frame)")
        #expect(!frame.contains("·"), "\(frame)")
    }

    /// C86 + A13.14's binary half — a QUEUED PAYLOAD HOLDING BYTES leaves as a
    /// binary frame `[4B len | head with _binary_path | body]`, python's
    /// `encode_outbound`. The fanout used to carry the header alone, so a
    /// watcher on a `write_stream` received a hole where the body was.
    @Test func watcherGetsBinaryFrameForBytesPayload() async throws {
        let kernel = try await startWebKernel()
        _ = await kernel.send([Item(AgentId("web"), .object(["type": .string("boot")]))])[0].result
        await mountWebWS(kernel)
        defer { Task { await stopWebHost(kernel) } }
        let port = kernel.httpPort()
        let task = URLSession(configuration: .ephemeral)
            .webSocketTask(with: URL(string: "ws://127.0.0.1:\(port)/kernel_state/ws")!)
        task.resume()
        defer { task.cancel(with: .normalClosure, reason: nil) }
        // The connection auto-watches `core`.
        try await Task.sleep(nanoseconds: 100_000_000)
        let blob = Data([0xDE, 0xAD, 0xBE, 0xEF])
        await kernel.emitItem(
            AgentId("kernel_state"),
            .binary(
                .object([
                    "type": .string("chunk"), "path": .string("a.bin"), "bytes": .null,
                ]), "bytes", blob))
        var frame = Data()
        for _ in 0..<5 {
            if case .data(let d) = try await task.receive() {
                frame = d
                break
            }
        }
        let decoded = try Codec.decodeBinaryFrame(frame)
        #expect(decoded.header["type"].asString == "event")
        #expect(decoded.header["_binary_path"].asString == "payload.bytes")
        #expect(decoded.header["payload"]["type"].asString == "chunk")
        #expect(decoded.body == blob)
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
        let task = session.webSocketTask(with: URL(string: "ws://127.0.0.1:\(port)/kernel_state/ws")!)
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

    // MARK: - C41 / A11.7 / A11.8 / A11.29 / A13.15 — the leg gate and the frame loop

    /// A `web_ws` leg with NO ingress rule (or a sealed one). The socket still
    /// upgrades; every frame on it is refused.
    @discardableResult
    private func mountSealedWS(_ kernel: Kernel) async -> String {
        let rec = await kernel.send([
            Item(
                AgentId("kernel"),
                .object([
                    "type": .string("create_agent"), "parent_id": .string("web"),
                    "handler_module": .string("web_ws.tools"), "id": .string("web_ws"),
                ]))
        ])[0].result
        let wsId = rec["id"].asString ?? "web_ws"
        _ = await kernel.send([
            Item(AgentId("web"), .object(["type": .string("mount"), "child_id": .string(wsId)]))
        ])[0].result
        return wsId
    }

    /// Read frames until `want` matches one, or the budget runs out.
    private func awaitFrame(
        _ task: URLSessionWebSocketTask, tries: Int = 6, _ want: (JSON) -> Bool
    ) async -> JSON? {
        for _ in 0..<tries {
            guard let msg = try? await task.receive() else { return nil }
            let text: String
            switch msg {
            case .string(let t): text = t
            case .data(let d): text = String(data: d, encoding: .utf8) ?? ""
            @unknown default: continue
            }
            guard let parsed = try? JSON.parse(text) else { continue }
            if want(parsed) { return parsed }
        }
        return nil
    }

    /// C41 — the CONNECT-TIME AUTO-WATCH IS GATED. Measured: the sealed swift
    /// socket delivered the host's boot/output/write/stop events to anyone who
    /// completed the handshake; python sends none.
    @Test func sealedLegSkipsAutowatch() async throws {
        let kernel = try await startWebKernel()
        _ = await kernel.send([Item(AgentId("web"), .object(["type": .string("boot")]))])[0].result
        await mountSealedWS(kernel)
        defer { Task { await stopWebHost(kernel) } }
        let port = kernel.httpPort()
        let task = URLSession(configuration: .ephemeral)
            .webSocketTask(with: URL(string: "ws://127.0.0.1:\(port)/kernel_state/ws")!)
        task.resume()
        defer { task.cancel(with: .normalClosure, reason: nil) }
        try await Task.sleep(nanoseconds: 150_000_000)
        await kernel.emit(
            AgentId("kernel_state"), .object(["type": .string("token"), "text": .string("leak")]))
        // Nothing may arrive. A receive left running for a window and then
        // abandoned is the pass — `URLSessionWebSocketTask.receive` does not
        // return on cancellation, so it is never AWAITED here.
        let seen = FrameBox()
        let probe = Task { if let m = try? await task.receive() { seen.set(m) } }
        try await Task.sleep(nanoseconds: 500_000_000)
        probe.cancel()
        #expect(!seen.got(), "a sealed leg auto-watched and leaked the host's events")
    }

    /// C41 — an `emit` frame on a sealed leg is DROPPED (python answers nothing
    /// at all for a denied emit). The kernel must never see the payload.
    @Test func emitOnSealedLegDropped() async throws {
        let kernel = try await startWebKernel()
        _ = await kernel.send([Item(AgentId("web"), .object(["type": .string("boot")]))])[0].result
        await mountSealedWS(kernel)
        defer { Task { await stopWebHost(kernel) } }
        let port = kernel.httpPort()
        let task = URLSession(configuration: .ephemeral)
            .webSocketTask(with: URL(string: "ws://127.0.0.1:\(port)/kernel_state/ws")!)
        task.resume()
        defer { task.cancel(with: .normalClosure, reason: nil) }
        let watcher = AgentId("emit_probe")
        let inbox = kernel.ensureInbox(watcher)
        _ = kernel.watch(src: AgentId("kernel_state"), watcher: watcher)
        try await task.send(
            .string(
                JSON.object([
                    "type": .string("emit"), "target": .string("kernel_state"),
                    "payload": .object(["type": .string("sealed")]),
                ]).serialize()))
        let seen = PayloadBox()
        let probe = Task { for await item in inbox { seen.set(item.payload) } }
        try await Task.sleep(nanoseconds: 500_000_000)
        probe.cancel()
        #expect(seen.get() == nil, "a denied emit reached the target: \(String(describing: seen.get()))")
    }

    /// A11.7 (r2 WSF-1) — every undecodable frame answers the ONE frame error,
    /// BEFORE any gate, and the socket survives to answer the next call.
    @Test func malformedFramesAnswerTheFrameErrorAndKeepTheSocket() async throws {
        let kernel = try await startWebKernel()
        _ = await kernel.send([Item(AgentId("web"), .object(["type": .string("boot")]))])[0].result
        await mountWebWS(kernel)
        defer { Task { await stopWebHost(kernel) } }
        let port = kernel.httpPort()
        let task = URLSession(configuration: .ephemeral)
            .webSocketTask(with: URL(string: "ws://127.0.0.1:\(port)/kernel_state/ws")!)
        task.resume()
        defer { task.cancel(with: .normalClosure, reason: nil) }
        // (a) text that is not JSON, (b) JSON that is not an object,
        // (f) a `_binary_path` python's `set_path` cannot place.
        let texts = ["{oops", "", "[1]", "\"call\"", "7"]
        for t in texts {
            try await task.send(.string(t))
            let err = await awaitFrame(task) { $0["type"].asString == "error" }
            let e = try #require(err, "no frame error for \(t.debugDescription)")
            #expect(
                e["error"].asString
                    == "frame: frame must be a well-formed call/emit/watch envelope", "\(e)")
            #expect(e["reason"].asString == "bad_arg" && e["arg"].asString == "frame", "\(e)")
            // Key order: type, error, reason, arg.
            #expect(
                Array(e.asObject?.keys ?? []) == ["type", "error", "reason", "arg"], "\(e)")
        }
        // (c) a binary frame under 4 bytes.
        try await task.send(.data(Data([1, 2, 3])))
        #expect(await awaitFrame(task) { $0["type"].asString == "error" } != nil)
        // (f) an unplaceable `_binary_path`.
        var head: JSON = .object([
            "type": .string("call"), "id": .string("b1"), "target": .string("kernel_state"),
            "payload": .object(["type": .string("reflect")]),
        ])
        head["_binary_path"] = .string("payload.nope.deep")
        try await task.send(.data(Codec.encodeBinaryFrame(header: head, body: Data("x".utf8))))
        #expect(await awaitFrame(task) { $0["type"].asString == "error" } != nil)
        // THE SOCKET SURVIVED: the next call is answered.
        try await task.send(
            .string(
                JSON.object([
                    "type": .string("call"), "id": .string("after"),
                    "target": .string("kernel_state"),
                    "payload": .object(["type": .string("list_agents")]),
                ]).serialize()))
        let reply = await awaitFrame(task) {
            $0["type"].asString == "reply" && $0["id"].asString == "after"
        }
        #expect(reply != nil, "the socket did not survive the malformed frames")
    }

    /// A11.8 — a `call` frame with no `payload` dispatches `{}`, which the
    /// KERNEL refuses `send: payload.type required`; the frame `id` defaults to
    /// `""` and is always echoed.
    @Test func callFrameWithoutPayloadAnswersPayloadTypeRequired() async throws {
        let kernel = try await startWebKernel()
        _ = await kernel.send([Item(AgentId("web"), .object(["type": .string("boot")]))])[0].result
        await mountWebWS(kernel)
        defer { Task { await stopWebHost(kernel) } }
        let port = kernel.httpPort()
        let task = URLSession(configuration: .ephemeral)
            .webSocketTask(with: URL(string: "ws://127.0.0.1:\(port)/kernel_state/ws")!)
        task.resume()
        defer { task.cancel(with: .normalClosure, reason: nil) }
        try await task.send(
            .string(
                JSON.object([
                    "type": .string("call"), "target": .string("kernel_state"),
                ]).serialize()))
        let reply = await awaitFrame(task) { $0["type"].asString == "reply" }
        let r = try #require(reply)
        #expect(r["id"].asString == "", "an absent frame id echoes as \"\": \(r)")
        #expect(r["data"]["reason"].asString == "missing_arg", "\(r)")
        #expect(r["data"]["error"].asString == "send: payload.type required", "\(r)")
    }

    /// A11.8 (r2 WSF-V4) — an `emit` frame with no payload emits `{}`, not
    /// `null`: swift's watchers received `{"type":"event","payload":null}`.
    @Test func emitFrameWithoutPayloadEmitsEmptyObject() async throws {
        let kernel = try await startWebKernel()
        _ = await kernel.send([Item(AgentId("web"), .object(["type": .string("boot")]))])[0].result
        await mountWebWS(kernel)
        defer { Task { await stopWebHost(kernel) } }
        let port = kernel.httpPort()
        let task = URLSession(configuration: .ephemeral)
            .webSocketTask(with: URL(string: "ws://127.0.0.1:\(port)/kernel_state/ws")!)
        task.resume()
        defer { task.cancel(with: .normalClosure, reason: nil) }
        try await Task.sleep(nanoseconds: 150_000_000)
        try await task.send(
            .string(
                JSON.object([
                    "type": .string("emit"), "target": .string("kernel_state"),
                ]).serialize()))
        let ev = await awaitFrame(task) { $0["type"].asString == "event" }
        let e = try #require(ev, "the auto-watch saw no event")
        #expect(e["payload"] == .object([:]), "\(e)")
    }

    /// A13.15 — a BINARY frame routes by `type` exactly like a text one, and its
    /// body reaches the target as the item's blob (never `sendWithBinary`, which
    /// skips the ledger, the dedup and the reflect flags).
    @Test func binaryFrameRoutesByTypeAndHonoursBinaryPath() async throws {
        let kernel = try await startWebKernel()
        _ = await kernel.send([Item(AgentId("web"), .object(["type": .string("boot")]))])[0].result
        let wsId = await mountWebWS(kernel)
        let rel = "fantastic-wsbin-\(UUID().uuidString)"
        let dir = URL(fileURLWithPath: FileManager.default.currentDirectoryPath)
            .appendingPathComponent(rel)
        try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: dir) }
        _ = await kernel.send([
            Item(
                AgentId("kernel"),
                .object([
                    "type": .string("create_agent"), "parent_id": .string("kernel_state"),
                    "handler_module": .string("file_bridge.tools"), "allow": ["."], "id": .string("wsfiles"),
                    "root": .string(rel), "ingress_rule": .string("allow_all_ingress"),
                    "callable_by": .object([
                        "$self": .string("*"), "$parent": .string("*"),
                        "$children": .string("*"), wsId: .string("*"),
                    ]),
                ]))
        ])[0].result
        _ = await kernel.send([
            Item(
                AgentId("kernel"),
                .object([
                    "type": .string("update_agent"), "id": .string(wsId),
                    "can_call": .object(["wsfiles": .string("*")]),
                ]))
        ])[0].result
        defer { Task { await stopWebHost(kernel) } }
        let port = kernel.httpPort()
        let task = URLSession(configuration: .ephemeral)
            .webSocketTask(with: URL(string: "ws://127.0.0.1:\(port)/kernel_state/ws")!)
        task.resume()
        defer { task.cancel(with: .normalClosure, reason: nil) }
        let blob = Data((0..<10240).map { UInt8($0 & 0xFF) })
        var head: JSON = .object([
            "type": .string("call"), "id": .string("w1"), "target": .string("wsfiles"),
            "payload": .object([
                "type": .string("write_stream"), "path": .string("x.bin"),
                "truncate": .bool(true),
            ]),
        ])
        head["_binary_path"] = .string("payload.bytes")
        try await task.send(.data(Codec.encodeBinaryFrame(header: head, body: blob)))
        let wrote = await awaitFrame(task) {
            $0["type"].asString == "reply" && $0["id"].asString == "w1"
        }
        let w = try #require(wrote, "no reply to the binary write frame")
        #expect(w["data"]["written"].asInt == Int64(blob.count), "\(w)")
        // …and the same bytes read back, as a BINARY reply frame whose head
        // names `data.bytes`.
        try await task.send(
            .string(
                JSON.object([
                    "type": .string("call"), "id": .string("r1"),
                    "target": .string("wsfiles"),
                    "payload": .object([
                        "type": .string("read_stream"), "path": .string("x.bin"),
                        "length": .integer(65536),
                    ]),
                ]).serialize()))
        var got: (header: JSON, body: Data)? = nil
        for _ in 0..<6 {
            guard let msg = try? await task.receive() else { break }
            if case .data(let d) = msg, let dec = try? Codec.decodeBinaryFrame(d),
                dec.header["id"].asString == "r1"
            {
                got = dec
                break
            }
        }
        let g = try #require(got, "read_stream did not answer a binary frame")
        #expect(g.header["_binary_path"].asString == "data.bytes", "\(g.header)")
        #expect(g.body == blob.prefix(g.body.count) && g.body.count > 0)
    }

    /// A11.29 + C77's WS half — `state_subscribe` answers a snapshot of EVERY
    /// agent, then streams `state_event` frames. Swift answered nothing at all.
    @Test func stateSubscribeSendsSnapshotThenEvents() async throws {
        let kernel = try await startWebKernel()
        _ = await kernel.send([Item(AgentId("web"), .object(["type": .string("boot")]))])[0].result
        let wsId = await mountWebWS(kernel)
        // The state stream is a KERNEL-WIDE read, gated on the ROOT: the leg
        // needs the `state_subscribe` edge NAMED, never a wildcard.
        _ = await kernel.send([
            Item(
                AgentId("kernel"),
                .object([
                    "type": .string("update_agent"), "id": .string(wsId),
                    "can_call": .object([
                        "kernel_state": .array([.string("*"), .string("state_subscribe")])
                    ]),
                ]))
        ])[0].result
        _ = await kernel.send([
            Item(
                AgentId("kernel_state"),
                .object([
                    "type": .string("update_agent"), "id": .string("kernel_state"),
                    "callable_by": .object([
                        "$self": .string("*"), "$parent": .string("*"),
                        "$children": .string("*"),
                        wsId: .array([.string("*"), .string("state_subscribe")]),
                    ]),
                ]))
        ])[0].result
        defer { Task { await stopWebHost(kernel) } }
        let port = kernel.httpPort()
        let task = URLSession(configuration: .ephemeral)
            .webSocketTask(with: URL(string: "ws://127.0.0.1:\(port)/kernel_state/ws")!)
        task.resume()
        defer { task.cancel(with: .normalClosure, reason: nil) }
        try await task.send(
            .string(
                JSON.object([
                    "type": .string("state_subscribe"), "id": .string("s1"),
                ]).serialize()))
        let snap = await awaitFrame(task) { $0["type"].asString == "state_snapshot" }
        let s = try #require(snap, "no state_snapshot")
        let agents = s["agents"].asArray ?? []
        #expect(agents.contains { $0["agent_id"].asString == "kernel_state" }, "\(s)")
        #expect(agents.allSatisfy { Array($0.asObject?.keys ?? []) == ["agent_id", "name", "backlog"] })
        // …then the event stream.
        await kernel.emit(
            AgentId("kernel_state"), .object(["type": .string("ping")]))
        let ev = await awaitFrame(task, tries: 10) { $0["type"].asString == "state_event" }
        let e = try #require(ev, "no state_event after the snapshot")
        #expect(Array(e.asObject?.keys ?? []).first == "type", "`type` leads: \(e)")
        #expect(e["name"].asString != nil, "the lazy name fill is missing: \(e)")
    }

    /// A11.29 (r2 WSF-5) — WITHOUT the root grant the leg gets python's
    /// FLATTENED denial frame, not a `data`-wrapped reply and not silence. The
    /// leg holds only the structural baseline: a NAMED `*` on the root would
    /// cover `state_subscribe` (audit 2026-09-28).
    @Test(.timeLimit(.minutes(1)))
    func stateSubscribeWithoutRootGrantAnswersFlattenedError() async throws {
        let kernel = try await startWebKernel()
        _ = await kernel.send([Item(AgentId("web"), .object(["type": .string("boot")]))])[0].result
        await mountWebWS(kernel, granted: false)
        defer { Task { await stopWebHost(kernel) } }
        let port = kernel.httpPort()
        let task = URLSession(configuration: .ephemeral)
            .webSocketTask(with: URL(string: "ws://127.0.0.1:\(port)/kernel_state/ws")!)
        task.resume()
        defer { task.cancel(with: .normalClosure, reason: nil) }
        try await task.send(
            .string(
                JSON.object([
                    "type": .string("state_subscribe"), "id": .string("x-s-1"),
                ]).serialize()))
        let err = await awaitFrame(task) {
            $0["type"].asString == "error" && $0["id"].asString == "x-s-1"
        }
        let e = try #require(err, "the ungranted state_subscribe answered nothing")
        #expect(e["id"].asString == "x-s-1", "\(e)")
        #expect(e["reason"].asString == "unauthorized", "\(e)")
        #expect(e["data"].isNull, "the denial is FLATTENED, not wrapped: \(e)")
    }

    /// A11.7 — the drain is a REGISTERED kernel task on the HOST agent, so an
    /// open socket is a `ps` row (`ws_drain`) and its pump is cancellable.
    @Test func openWsShowsWsDrainInPs() async throws {
        let kernel = try await startWebKernel()
        _ = await kernel.send([Item(AgentId("web"), .object(["type": .string("boot")]))])[0].result
        await mountWebWS(kernel)
        defer { Task { await stopWebHost(kernel) } }
        let port = kernel.httpPort()
        let task = URLSession(configuration: .ephemeral)
            .webSocketTask(with: URL(string: "ws://127.0.0.1:\(port)/kernel_state/ws")!)
        task.resume()
        defer { task.cancel(with: .normalClosure, reason: nil) }
        try await Task.sleep(nanoseconds: 200_000_000)
        let ps = await kernel.send([
            Item(AgentId("kernel"), .object(["type": .string("ps")]))
        ])[0].result
        let verbs = (ps["tasks"].asArray ?? []).compactMap { $0["verb"].asString }
        #expect(verbs.contains("ws_drain"), "\(ps)")
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
            "GET /kernel_state/ws HTTP/1.1\r\nHost: h\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n"
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

    /// ONE WRITER PER CONNECTION (python has exactly one). `writeAndFlush` off
    /// the event loop only ENQUEUES, so a frame written straight from
    /// `channelRead` — the frame error here, and equally a denial, a watch
    /// refusal or the `state_snapshot` — ran ON the loop and could land BETWEEN
    /// the event a call had flushed and that call's own reply. Python cannot
    /// produce that interleave. 50 rounds on ONE socket, each interleaving an
    /// undecodable frame with a call: the frame immediately before every reply
    /// is that call's event, and the error is never inside the pair.
    @Test func everyOutboundFrameRidesTheOneWriter() async throws {
        let kernel = try await startWebKernel()
        _ = await kernel.send([Item(AgentId("web"), .object(["type": .string("boot")]))])[0].result
        await mountWebWS(kernel)
        defer { Task { await stopWebHost(kernel) } }
        let port = kernel.httpPort()
        let task = URLSession(configuration: .ephemeral)
            .webSocketTask(with: URL(string: "ws://127.0.0.1:\(port)/kernel_state/ws")!)
        task.resume()
        defer { task.cancel(with: .normalClosure, reason: nil) }
        for i in 0..<50 {
            let id = "one\(i)"
            // (1) undecodable — answered from `channelRead`, on the event loop;
            // (2) a call — flushed AND replied from its own `ws_call` task.
            try await task.send(.string("{oops \(i)"))
            try await task.send(
                .string(
                    JSON.object([
                        "type": .string("call"), "id": .string(id),
                        "target": .string("kernel_state"),
                        "payload": .object(["type": .string("list_agents")]),
                    ]).serialize()))
            var seen: [String] = []
            while true {
                let got = try await Self.receiveJSON(task)
                seen.append(got["type"].asString ?? "?")
                if got["type"].asString == "reply" {
                    #expect(got["id"].asString == id, "round \(i): \(got.serialize())")
                    break
                }
            }
            #expect(seen.contains("error"), "round \(i): no frame error: \(seen)")
            #expect(seen.contains("event"), "round \(i): no fanout event: \(seen)")
            #expect(
                seen.dropLast().last == "event",
                "round \(i): a frame interleaved between the flush and the reply: \(seen)")
        }
    }

    /// A13.19 ON THE WS LEG — a TEXT `call` carries no raw chunk, so python
    /// leaves the client's JSON `bytes` value in the payload and `file_bridge`
    /// TYPES it: `missing_arg`, nothing written. Swift stripped the key before
    /// dispatch (python overwrites it only when a BODY actually arrived), so the
    /// refusal became a silent 0-byte write. An ABSENT `bytes` is python's
    /// `payload.get("bytes", b"")` — a legal empty chunk.
    @Test func textCallWriteStreamTypesItsBytesArgument() async throws {
        let kernel = try await startWebKernel()
        _ = await kernel.send([Item(AgentId("web"), .object(["type": .string("boot")]))])[0].result
        let wsId = await mountWebWS(kernel)
        let rel = "fantastic-wswrite-\(UUID().uuidString)"
        let dir = URL(fileURLWithPath: FileManager.default.currentDirectoryPath)
            .appendingPathComponent(rel)
        try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: dir) }
        // A CHILD of the leg: `$children` (egress) + `$parent` (ingress) are the
        // structural baseline, so no explicit grant is needed either way.
        _ = await kernel.send([
            Item(
                AgentId("kernel"),
                .object([
                    "type": .string("create_agent"), "parent_id": .string(wsId),
                    "handler_module": .string("file_bridge.tools"), "allow": ["."], "id": .string("wsfiles"),
                    "root": .string(rel), "ingress_rule": .string("allow_all_ingress"),
                ]))
        ])[0].result
        defer { Task { await stopWebHost(kernel) } }
        let port = kernel.httpPort()
        let task = URLSession(configuration: .ephemeral)
            .webSocketTask(with: URL(string: "ws://127.0.0.1:\(port)/kernel_state/ws")!)
        task.resume()
        defer { task.cancel(with: .normalClosure, reason: nil) }
        func call(_ payload: JSON, _ id: String) async throws -> JSON {
            try await task.send(
                .string(
                    JSON.object([
                        "type": .string("call"), "id": .string(id),
                        "target": .string("wsfiles"), "payload": payload,
                    ]).serialize()))
            let got = await awaitFrame(task) {
                $0["type"].asString == "reply" && $0["id"].asString == id
            }
            return try #require(got)["data"]
        }
        let refused = try await call(
            .object([
                "type": .string("write_stream"), "path": .string("w.txt"),
                "bytes": .string("abc"),
            ]), "ws1")
        #expect(refused["reason"].asString == "missing_arg", "\(refused)")
        #expect(
            refused["error"].asString == "write_stream: bytes (one raw chunk) required",
            "\(refused)")
        #expect(
            !FileManager.default.fileExists(atPath: dir.appendingPathComponent("w.txt").path),
            "the refused write still wrote")
        // ABSENT is the empty chunk python writes.
        let empty = try await call(
            .object(["type": .string("write_stream"), "path": .string("e.txt")]), "ws2")
        #expect(empty["written"].asInt == 0, "\(empty)")
        #expect((try? Data(contentsOf: dir.appendingPathComponent("e.txt")))?.count == 0, "\(empty)")
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

/// A one-shot box for "did anything arrive in the window?" probes — a WS
/// receive cannot be awaited under cancellation, so the probe writes here and
/// the test reads after its own sleep.
final class FrameBox: @unchecked Sendable {
    private let lock = NSLock()
    private var value: URLSessionWebSocketTask.Message?
    func set(_ m: URLSessionWebSocketTask.Message) {
        lock.lock()
        defer { lock.unlock() }
        if value == nil { value = m }
    }
    func got() -> Bool {
        lock.lock()
        defer { lock.unlock() }
        return value != nil
    }
}

/// The same, for an inbox payload.
final class PayloadBox: @unchecked Sendable {
    private let lock = NSLock()
    private var value: JSON?
    func set(_ p: JSON) {
        lock.lock()
        defer { lock.unlock() }
        if value == nil { value = p }
    }
    func get() -> JSON? {
        lock.lock()
        defer { lock.unlock() }
        return value
    }
}
