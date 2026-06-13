// Remote kernel_bridge transport — WebSocket over URLSession.
//
// The bridge is WS-only (asymmetric client): it opens a WS to the
// remote kernel's `web_ws` endpoint and ships **raw call frames** —
// `{type:"call", id, target, payload}`. The remote's `web_ws`
// dispatches via `kernel.send(target, payload)` exactly like a
// browser frame; the matching `{type:"reply", id, data}` flows
// back. No B-side bridge agent needed.
//
// Streams use the same WS protocol's watch frames:
//   - outbound: `{type:"watch", src:<target>}` / `{type:"unwatch", src}`
//   - inbound:  `{type:"event", payload}` — re-emitted on the local
//     bridge agent's inbox so local watchers see remote streams via
//     standard `kernel.watch(<bridge_id>, ...)`.

import FantasticIoBridge
import FantasticJSON
import FantasticKernel
import Foundation

// ── WebSocket transport ────────────────────────────────────────────

public actor WebSocketTransport {
    private let endpoint: URL
    private var task: URLSessionWebSocketTask?
    private let session: URLSession

    /// Outstanding requests keyed by id; resumed when the remote
    /// echoes the reply back with the same id.
    private var pending: [String: CheckedContinuation<JSON, Never>] = [:]
    /// Binary forwards in flight — resolved with `(reply, body)` when the
    /// remote echoes a `reply` (a `read_stream` reply arrives as a codec
    /// binary frame carrying raw bytes; a `write_stream` status arrives as a
    /// plain text `reply` with an empty body).
    private var binaryPending: [String: CheckedContinuation<(JSON, Data), Never>] = [:]
    private var nextId: UInt64 = 1

    /// Per-forward timeout — a forward whose reply/error never arrives
    /// fails after this rather than hanging forever (matches Python's
    /// 30s DEFAULT_FORWARD_TIMEOUT).
    private let forwardTimeoutSeconds: Double = 30.0

    /// Set by the bundle after `attachWebSocket(agentId:endpoint:kernel:)`.
    /// Inbound `event` frames re-emit `payload` on this agent's local
    /// inbox via `kernel.emit(eventSink, payload)`. nil → events
    /// are dropped (the transport doesn't know which agent to emit
    /// on; used by tests that exercise forward without streams).
    private var eventSink: AgentId?
    private weak var kernel: Kernel?

    public init(endpoint: URL, session: URLSession = .shared) {
        self.endpoint = endpoint
        self.session = session
    }

    public func setEventSink(agentId: AgentId, kernel: Kernel) {
        self.eventSink = agentId
        self.kernel = kernel
    }

    public func connect() async {
        let t = session.webSocketTask(with: endpoint)
        self.task = t
        t.resume()
        // Receive loop — pumps incoming frames into `pending` /
        // emits `event` frames on the bridge agent's local inbox.
        Task { [weak self] in
            await self?.receiveLoop()
        }
    }

    public func close() {
        task?.cancel(with: .normalClosure, reason: nil)
        task = nil
        // Resume any in-flight callers with a transport-error.
        for (_, cont) in pending {
            cont.resume(
                returning: .object([
                    "error": .string("websocket closed"),
                    "reason": .string("transport_closed"),
                ]))
        }
        pending.removeAll()
        for (_, cont) in binaryPending {
            cont.resume(
                returning: (
                    .object([
                        "error": .string("websocket closed"),
                        "reason": .string("transport_closed"),
                    ]), Data()
                ))
        }
        binaryPending.removeAll()
    }

    public func forward(target: AgentId, payload: JSON) async -> JSON {
        guard let task = task else {
            return .object([
                "error": .string("websocket not connected"),
                "reason": .string("not_connected"),
            ])
        }
        let id = mintId()
        let frame: JSON = .object([
            "type": .string("call"),
            "id": .string(id),
            "target": .string(target.value),
            "payload": payload,
        ])
        let msg = URLSessionWebSocketTask.Message.string(frame.serialize())
        do {
            try await task.send(msg)
        } catch {
            return .object([
                "error": .string("send failed: \(error)"),
                "reason": .string("transport_error"),
            ])
        }
        return await withCheckedContinuation { cont in
            pending[id] = cont
            // Guard against an indefinite hang: if no reply/error/drop
            // resolves this within the timeout, fail it. The actor
            // serializes `pending`, so exactly one of {reply, error,
            // drop, timeout} resumes the continuation.
            Task { await self.timeoutPending(id: id) }
        }
    }

    private func timeoutPending(id: String) async {
        try? await Task.sleep(nanoseconds: UInt64(forwardTimeoutSeconds * 1_000_000_000))
        if let cont = pending.removeValue(forKey: id) {
            cont.resume(
                returning: .object([
                    "error": .string(
                        "kernel_bridge.forward: timeout after \(Int(forwardTimeoutSeconds))s"),
                    "reason": .string("timeout"),
                ]))
        }
    }

    /// Binary forward over the WIRE — a `read_stream`/`write_stream` chunk
    /// carried cross-kernel as a codec binary frame `[4B len | header | body]`.
    /// The header is the inner call (`{type:read_stream,…}`) with `target`/`id`
    /// stamped on so the remote `web_ws` can route + correlate; the trailing
    /// bytes are the raw body (never base64). The reply arrives either as a
    /// codec binary frame (read_stream → raw bytes) or a plain text `reply`
    /// (write_stream status → empty body) — both resolve `binaryPending[id]`.
    public func binaryForward(target: AgentId, header: JSON, blob: Data) async -> (JSON, Data) {
        guard let task = task else {
            return (
                .object([
                    "error": .string("websocket not connected"),
                    "reason": .string("not_connected"),
                ]), Data()
            )
        }
        let id = mintId()
        // The STANDARD call envelope (py/rust parity) — the inner call rides in
        // `payload`, exactly like a text forward; `_binary_path` tells a py/rust
        // receiver where the trailing raw body belongs. A flattened inner-call
        // header is swift-only dialect and breaks cross-runtime streams.
        let wire: JSON = .object([
            "type": .string("call"),
            "id": .string(id),
            "target": .string(target.value),
            "payload": header,
            "_binary_path": .string("payload.bytes"),
        ])
        let frame = Codec.encodeBinaryFrame(header: wire, body: blob)
        do {
            try await task.send(.data(frame))
        } catch {
            return (
                .object([
                    "error": .string("send failed: \(error)"),
                    "reason": .string("transport_error"),
                ]), Data()
            )
        }
        return await withCheckedContinuation { cont in
            binaryPending[id] = cont
            Task { await self.timeoutBinaryPending(id: id) }
        }
    }

    private func timeoutBinaryPending(id: String) async {
        try? await Task.sleep(nanoseconds: UInt64(forwardTimeoutSeconds * 1_000_000_000))
        if let cont = binaryPending.removeValue(forKey: id) {
            cont.resume(
                returning: (
                    .object([
                        "error": .string(
                            "kernel_bridge.binaryForward: timeout after \(Int(forwardTimeoutSeconds))s"
                        ),
                        "reason": .string("timeout"),
                    ]), Data()
                ))
        }
    }

    /// Send a `{type:"watch", src:<target>}` frame so the remote
    /// starts pushing events for `target` back over this WS. The
    /// inbound `event` frames are re-emitted on the bridge agent's
    /// local inbox via `setEventSink`.
    public func watchRemote(target: AgentId) async -> JSON {
        guard let task = task else {
            return .object([
                "error": .string("websocket not connected"),
                "reason": .string("not_connected"),
            ])
        }
        let frame: JSON = .object([
            "type": .string("watch"),
            "src": .string(target.value),
        ])
        do {
            try await task.send(.string(frame.serialize()))
        } catch {
            return .object([
                "error": .string("send failed: \(error)"),
                "reason": .string("transport_error"),
            ])
        }
        return .object([
            "ok": .bool(true),
            "watching": .string(target.value),
        ])
    }

    /// Symmetric teardown for `watchRemote`. Events already in
    /// flight on the wire still arrive + re-emit.
    public func unwatchRemote(target: AgentId) async -> JSON {
        guard let task = task else {
            return .object([
                "error": .string("websocket not connected"),
                "reason": .string("not_connected"),
            ])
        }
        let frame: JSON = .object([
            "type": .string("unwatch"),
            "src": .string(target.value),
        ])
        do {
            try await task.send(.string(frame.serialize()))
        } catch {
            return .object([
                "error": .string("send failed: \(error)"),
                "reason": .string("transport_error"),
            ])
        }
        return .object([
            "ok": .bool(true),
            "unwatched": .string(target.value),
        ])
    }

    private func receiveLoop() async {
        guard let task = task else { return }
        while true {
            do {
                let msg = try await task.receive()
                switch msg {
                case .string(let s):
                    if let parsed = try? JSON.parse(s) {
                        await handleInboundFrame(parsed, body: Data())
                    }
                case .data(let d):
                    // A codec binary frame (`[4B len|header|body]`) carries a
                    // read_stream reply's raw bytes. Plain JSON delivered as
                    // `.data` fails decodeBinaryFrame (its length prefix is
                    // garbage), so fall back to a UTF-8 parse.
                    if let (header, body) = Codec.decodeBinaryFrame(d) {
                        await handleInboundFrame(header, body: body)
                    } else if let text = String(data: d, encoding: .utf8),
                        let parsed = try? JSON.parse(text)
                    {
                        await handleInboundFrame(parsed, body: Data())
                    }
                @unknown default: continue
                }
            } catch {
                break
            }
        }
        // Connection died; wake any pending (text + binary).
        for (_, cont) in pending {
            cont.resume(
                returning: .object([
                    "error": .string("websocket connection dropped"),
                    "reason": .string("transport_dropped"),
                ]))
        }
        pending.removeAll()
        for (_, cont) in binaryPending {
            cont.resume(
                returning: (
                    .object([
                        "error": .string("websocket connection dropped"),
                        "reason": .string("transport_dropped"),
                    ]), Data()
                ))
        }
        binaryPending.removeAll()
    }

    /// Route one decoded inbound frame to a pending forward (text or binary)
    /// or the event sink. `body` is the raw trailing bytes of a binary frame
    /// (empty for text frames).
    private func handleInboundFrame(_ parsed: JSON, body: Data) async {
        let ftype = parsed["type"].asString
        if ftype == "reply", let id = parsed["id"].asString {
            // Reply envelope is `{type:"reply", id, data}` — matches Python's
            // web/_proxy.py and the Swift web server (FantasticWeb/WebSocket).
            if let cont = pending.removeValue(forKey: id) {
                cont.resume(returning: parsed["data"])
            } else if let cont = binaryPending.removeValue(forKey: id) {
                // read_stream → body carries raw bytes; write_stream status →
                // text reply with an empty body.
                cont.resume(returning: (parsed["data"], body))
            }
        } else if ftype == "error", let id = parsed["id"].asString {
            // The remote's web_ws emits `{type:"error", id, error}` when its
            // dispatch RAISES. Fail the pending forward promptly. Matches
            // Python/Rust.
            let err: JSON = .object([
                "error": .string("remote error: \(parsed["error"].asString ?? "unknown")")
            ])
            if let cont = pending.removeValue(forKey: id) {
                cont.resume(returning: err)
            } else if let cont = binaryPending.removeValue(forKey: id) {
                cont.resume(returning: (err, Data()))
            }
        } else if ftype == "event" {
            // Re-emit on the bridge's local inbox so local watchers
            // (`kernel.watch(bridge_id, ...)`) see the remote stream.
            if let sink = eventSink, let kernel = kernel {
                await kernel.emit(sink, parsed["payload"])
            }
        }
    }

    private func mintId() -> String {
        nextId &+= 1
        return "br_\(nextId)"
    }
}

// ── Bundle extension: attach + dispatch via transport ──────────────

extension KernelBridgeBundle {
    /// Attach a WebSocket transport for `agentId`. Connection is
    /// established eagerly. The transport's event sink is wired so
    /// inbound `{type:"event"}` frames re-emit on the bridge's local
    /// inbox via `kernel.emit(agentId, payload)`.
    public func attachWebSocket(agentId: AgentId, endpoint: URL, kernel: Kernel) async {
        let ws = WebSocketTransport(endpoint: endpoint)
        await ws.setEventSink(agentId: agentId, kernel: kernel)
        await ws.connect()
        attachTransport(agentId: agentId, transport: .ws(ws))
    }
}
