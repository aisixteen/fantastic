// Remote kernel_bridge transports — WebSocket + HTTP over URLSession.
//
// Mirrors Rust's `fantastic-kernel-bridge` non-in-memory transports.
// The bundle's `forward` verb routes via whichever transport is
// attached to the bridge agent.

import FantasticJSON
import FantasticKernel
import Foundation

// ── HTTP transport ─────────────────────────────────────────────────

public actor HttpTransport {
    private let endpoint: URL
    private let session: URLSession

    public init(endpoint: URL, session: URLSession = .shared) {
        self.endpoint = endpoint
        self.session = session
    }

    public func forward(target: AgentId, payload: JSON) async -> JSON {
        // Matches Python `kernel_bridge._transport.HTTPTransport`
        // (the reference template): POST to `<endpoint>/<target>`
        // with the payload as the raw JSON body. Target lives in
        // the URL path, not in an envelope around the payload.
        let url = endpoint.appendingPathComponent(target.value)
        var req = URLRequest(url: url)
        req.httpMethod = "POST"
        req.setValue("application/json", forHTTPHeaderField: "Content-Type")
        req.httpBody = payload.serialize().data(using: .utf8)

        do {
            let (data, response) = try await session.data(for: req)
            if let http = response as? HTTPURLResponse, http.statusCode >= 400 {
                return .object([
                    "error": .string("HTTP \(http.statusCode) from \(endpoint)"),
                    "reason": .string("remote_http_error"),
                ])
            }
            return (try? JSON.parse(data))
                ?? .object(["error": .string("non-JSON reply from \(endpoint)")])
        } catch {
            return .object([
                "error": .string("transport: \(error)"),
                "reason": .string("transport_error"),
            ])
        }
    }
}

// ── WebSocket transport ────────────────────────────────────────────

public actor WebSocketTransport {
    private let endpoint: URL
    private var task: URLSessionWebSocketTask?
    private let session: URLSession

    /// Outstanding requests keyed by id; resumed when the remote
    /// echoes the reply back with the same id.
    private var pending: [String: CheckedContinuation<JSON, Never>] = [:]
    private var nextId: UInt64 = 1

    public init(endpoint: URL, session: URLSession = .shared) {
        self.endpoint = endpoint
        self.session = session
    }

    public func connect() async {
        let t = session.webSocketTask(with: endpoint)
        self.task = t
        t.resume()
        // Receive loop — pumps incoming frames into `pending`.
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
        }
    }

    private func receiveLoop() async {
        guard let task = task else { return }
        while true {
            do {
                let msg = try await task.receive()
                let text: String
                switch msg {
                case .string(let s): text = s
                case .data(let d): text = String(data: d, encoding: .utf8) ?? ""
                @unknown default: continue
                }
                guard let parsed = try? JSON.parse(text) else { continue }
                if parsed["type"].asString == "reply",
                    let id = parsed["id"].asString
                {
                    if let cont = pending.removeValue(forKey: id) {
                        // Reply envelope is `{type:"reply", id, data}` —
                        // matches Python's web/_proxy.py (reference
                        // template) and the Swift web server itself
                        // (FantasticWeb/WebSocket.swift:170-172).
                        cont.resume(returning: parsed["data"])
                    }
                }
            } catch {
                break
            }
        }
        // Connection died; wake any pending.
        for (_, cont) in pending {
            cont.resume(
                returning: .object([
                    "error": .string("websocket connection dropped"),
                    "reason": .string("transport_dropped"),
                ]))
        }
        pending.removeAll()
    }

    private func mintId() -> String {
        nextId &+= 1
        return "br_\(nextId)"
    }
}

// ── Bundle extension: attach + dispatch via transports ─────────────

extension KernelBridgeBundle {
    /// Attach an HTTP transport for the given bridge agent id. The
    /// app calls this after creating the `kernel_bridge.tools`
    /// agent.
    public func attachHttp(agentId: AgentId, endpoint: URL) {
        attachTransport(agentId: agentId, transport: .http(HttpTransport(endpoint: endpoint)))
    }

    /// Attach a WebSocket transport. Connection is established
    /// eagerly.
    public func attachWebSocket(agentId: AgentId, endpoint: URL) async {
        let ws = WebSocketTransport(endpoint: endpoint)
        await ws.connect()
        attachTransport(agentId: agentId, transport: .ws(ws))
    }
}
