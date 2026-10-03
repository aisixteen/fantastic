// ANY verb whose reply carries raw bytes answers on the binary channel.
//
// There is no list of binary verbs. A bundle's own verb — here `events`, the shape
// of a terminal-output poll — is called by a TEXT call (no bytes in the call); its
// reply holds raw bytes, so every leg answers it binary, because the reply carries
// a body and for no other reason:
//
//   (a) web_ws: a BINARY reply frame, the body at its `_binary_path` array; the
//       same verb answering no bytes is a TEXT frame;
//   (b) web_rest: an octet reply, the field named by `X-Fantastic-Bytes-Field`,
//       the rest of the reply on `X-Fantastic-Payload`; no bytes ⇒ JSON;
//   (c) a ws_bridge `forward` through web_ws: the call crosses as TEXT, the reply
//       as BINARY, and `forward` returns the bytes;
//   (d) a peer kernel's bridge ENGINE serving the inbound call (host web_rest →
//       `P-g|probe` over the host's web_ws): text call out, binary reply back,
//       octet out of REST;
//   (e) bytes at a NESTED key: web_ws carries the full path; web_rest's octet
//       names ONE top-level field, so a nested body has no REST form and is
//       refused as a stated transport error (500).
//
// Twin of python `io_bridge/tests/test_any_verb_bytes_reply.py`. Python's reply
// dict holds the bytes; swift's carrier is the reply's body (`Blob`), returned by
// the probe's `handleBinary` at its slot of the reply. The probe declares nothing.

import FantasticIoBridge
import FantasticJSON
import FantasticWeb
import FantasticWebRest
import FantasticWebWS
import Foundation
import OrderedCollections
import Testing

@testable import FantasticKernel
@testable import FantasticKernelBridge

// ─── the probe: a bundle's own verbs, nothing declared ──────────────────────

private let CHUNK = Data([0x1B]) + Data("[31mred".utf8) + Data([0x1B]) + Data("[0m".utf8)
    + Data([0x00, 0xFF, 0x0D, 0x0A])

private final class Probe: AgentBundle, @unchecked Sendable {
    let name = "anyverb_bytes_probe"
    private let lock = NSLock()
    private var _calls: [JSON] = []
    /// Every payload a verb of this probe was dispatched with.
    var calls: [String] { lock.withLock { _calls.map { $0["type"].asString ?? "" } } }

    /// The text channel holds no bytes: it answers the reply without its body.
    func handle(agentId: AgentId, payload: JSON, kernel: Kernel) async throws -> JSON? {
        try await handleBinary(agentId: agentId, header: payload, body: nil, kernel: kernel).0
    }

    func handleBinary(agentId: AgentId, header: JSON, body: Blob?, kernel: Kernel) async throws
        -> (JSON?, Blob?)
    {
        let verb = header["type"].asString ?? ""
        if verb == "boot" || verb == "shutdown" { return (nil, nil) }
        if verb == "reflect" {
            return (
                [
                    "id": .string(agentId.value), "sentence": "bytes-reply probe",
                    "verbs": .object([:]),
                ], nil
            )
        }
        lock.withLock { _calls.append(header) }
        switch verb {
        case "events":
            return (["seq": 3, "chunk": .null], Blob("chunk", CHUNK))
        case "idle":
            return (["seq": 3], nil)
        case "nested":
            return (["seq": 3, "out": ["chunk": .null]], Blob(parts: ["out", "chunk"], CHUNK))
        case "listed":
            return (
                ["events": [["n": 0], ["n": 1, "bytes": .null]]],
                Blob(parts: ["events", 1, "bytes"], CHUNK)
            )
        default:
            return (["error": .string("probe: no verb \(pyRepr(.string(verb)))")], nil)
        }
    }
}

// ─── (a) web_ws, (b) web_rest, (e) nested — one kernel ──────────────────────

/// `web` + an open web_ws leg `t_ws` and web_rest leg `t_rest`, each granted the
/// probe; the probe callable by both. Returns the kernel, its port and the probe.
private func one() async -> (Kernel, UInt16, Probe) {
    let probe = Probe()
    let r = BundleRegistry()
    r.register("web.tools", WebBundle())
    r.register("web_ws.tools", WebWSBundle())
    r.register("web_rest.tools", WebRestBundle())
    r.register("anyverb_bytes_probe.tools", probe)
    let k = Kernel(storage: .inMemory, bundles: r)
    let root = Agent(id: AgentId("kernel_state"), handlerModule: nil, parentId: nil)
    k.register(root)
    k.setRoot(root)
    for p: JSON in [
        [
            "type": "create_agent", "parent_id": "kernel_state", "handler_module": "web.tools",
            "id": "web", "port": .integer(Int64(freeTCPPort())),
        ],
        [
            "type": "create_agent", "parent_id": "web", "handler_module": "web_ws.tools",
            "id": "t_ws", "ingress_rule": "allow_all_ingress", "can_call": ["probe": "*"],
        ],
        [
            "type": "create_agent", "parent_id": "web", "handler_module": "web_rest.tools",
            "id": "t_rest", "ingress_rule": "allow_all_ingress", "can_call": ["probe": "*"],
        ],
        [
            "type": "create_agent", "parent_id": "kernel_state",
            "handler_module": "anyverb_bytes_probe.tools", "id": "probe",
            "callable_by": ["t_ws": "*", "t_rest": "*"],
        ],
    ] {
        let made = await k.send([Item(AgentId("kernel"), p)])[0].result
        #expect(made["error"].isNull, "stage: \(made.serialize())")
    }
    for leg in ["t_ws", "t_rest"] {
        // explicit edges: the host reads its leg's routes; the leg reaches the root
        wire(k, "web", leg)
        wire(k, leg, "kernel_state")
        _ = await k.send([Item(AgentId("web"), .object(["type": "mount", "child_id": .string(leg)]))])
    }
    return (k, k.httpPort(), probe)
}

private func stopWeb(_ k: Kernel) async {
    _ = await k.send([Item(AgentId("web"), .object(["type": "stop"]))])
}

private func socket(_ port: UInt16) -> URLSessionWebSocketTask {
    let task = URLSession(configuration: .ephemeral)
        .webSocketTask(with: URL(string: "ws://127.0.0.1:\(port)/web/ws")!)
    task.resume()
    return task
}

/// Send a TEXT call frame; return the reply frame for `fid` as it crossed — its
/// head (with `_binary_path`) and its raw body, `nil` for a TEXT frame (an
/// auto-watch `event` may precede it). BOUNDED: past 5 s the socket is cancelled.
private func wsCall(
    _ ws: URLSessionWebSocketTask, _ fid: String, _ payload: JSON
) async throws -> (JSON, Data?) {
    let frame: JSON = ["type": "call", "id": .string(fid), "target": "probe", "payload": payload]
    try await ws.send(.string(frame.serialize()))
    let timer = Task {
        try await Task.sleep(nanoseconds: 5_000_000_000)
        ws.cancel(with: .goingAway, reason: nil)
    }
    defer { timer.cancel() }
    while true {
        let (head, body): (JSON, Data?)
        switch try await ws.receive() {
        case .string(let s): (head, body) = (try JSON.parse(s), nil)
        case .data(let d):
            let f = try Codec.decodeBinaryFrame(d)
            (head, body) = (f.header, f.body)
        @unknown default: continue
        }
        if head["type"].asString == "reply", head["id"].asString == fid { return (head, body) }
    }
}

/// A JSON POST to `t_rest/probe`: status, raw body, content type and the two
/// octet headers.
private func restCall(_ port: UInt16, _ target: String, _ leg: String, _ payload: JSON)
    async throws -> (status: Int, body: Data, type: String?, field: String?, rest: String?)
{
    var req = URLRequest(url: URL(string: "http://127.0.0.1:\(port)/\(leg)/\(target)")!)
    req.httpMethod = "POST"
    req.setValue("application/json", forHTTPHeaderField: "Content-Type")
    req.httpBody = Data(payload.serialize().utf8)
    let (d, resp) = try await URLSession(configuration: .ephemeral).data(for: req)
    let http = resp as? HTTPURLResponse
    return (
        http?.statusCode ?? 0, d, http?.value(forHTTPHeaderField: "Content-Type"),
        http?.value(forHTTPHeaderField: "X-Fantastic-Bytes-Field"),
        http?.value(forHTTPHeaderField: "X-Fantastic-Payload")
    )
}

// ─── two kernels ─────────────────────────────────────────────────────────────

/// A kernel whose root carries `guid`, serving `web` on a free port, with the
/// bundles both sides need. Returns the kernel, its probe and its ws_bridge bundle.
private func kernel(guid: String) async -> (Kernel, Probe, KernelBridgeBundle) {
    let probe = Probe()
    let bridge = KernelBridgeBundle(family: .ws)
    let r = BundleRegistry()
    r.register("web.tools", WebBundle())
    r.register("web_ws.tools", WebWSBundle())
    r.register("web_rest.tools", WebRestBundle())
    r.register("ws_bridge.tools", bridge)
    r.register("anyverb_bytes_probe.tools", probe)
    let k = Kernel(storage: .inMemory, bundles: r)
    let root = Agent(id: AgentId("kernel_state"), handlerModule: nil, parentId: nil)
    k.register(root)
    k.setRoot(root)
    k.update(root.id, ["guid": .string(guid)])
    return (k, probe, bridge)
}

private func create(_ k: Kernel, _ fields: OrderedDictionary<String, JSON>) async -> String {
    var p: OrderedDictionary<String, JSON> = ["type": "create_agent"]
    for (key, v) in fields { p[key] = v }
    let out = await k.send([Item(AgentId("kernel"), .object(p))])[0]
    #expect(out.status == .completed, "\(out.result.serialize())")
    return out.result["id"].asString ?? ""
}

/// `web` on a free port with the given legs mounted under it. Returns the port.
private func serveWeb(_ k: Kernel, legs: [OrderedDictionary<String, JSON>]) async -> UInt16 {
    _ = await create(
        k,
        [
            "parent_id": "kernel_state", "handler_module": "web.tools", "id": "web",
            "port": .integer(Int64(freeTCPPort())),
        ])
    for var leg in legs {
        leg["parent_id"] = "web"
        let id = await create(k, leg)
        wire(k, "web", id)
        wire(k, id, "kernel_state")
        _ = await k.send([Item(AgentId("web"), .object(["type": "mount", "child_id": .string(id)]))])
    }
    return k.httpPort()
}

/// One frame as it crossed a bridge leg's socket: `in` is toward web_ws, `out`
/// from it (python `_WsEnd.wire`); `text` or `binary`; its head; its raw body.
private struct Crossed: Sendable {
    let dir: String
    let kind: String
    let head: JSON
    let body: Data?
}

/// A bridge leg's transport that keeps every frame crossing it, both ways — the
/// test's capture of the wire, around the real WS transport.
private final class WireTap: BridgeTransport, @unchecked Sendable {
    let inner: any BridgeTransport
    private let lock = NSLock()
    private var _wire: [Crossed] = []
    init(_ inner: any BridgeTransport) { self.inner = inner }
    private func keep(_ c: Crossed) { lock.withLock { _wire.append(c) } }
    /// The call/reply frames that crossed (hellos, watches and events aside).
    var callsAndReplies: [Crossed] {
        lock.withLock {
            _wire.filter { ["call", "reply"].contains($0.head["type"].asString ?? "") }
        }
    }
    func isClosed() async -> Bool { await inner.isClosed() }
    func send(_ frame: JSON) async throws {
        keep(Crossed(dir: "in", kind: "text", head: frame, body: nil))
        try await inner.send(frame)
    }
    func sendBinary(header: JSON, body: Data) async throws {
        keep(Crossed(dir: "in", kind: "binary", head: header, body: body))
        try await inner.sendBinary(header: header, body: body)
    }
    func recv() async throws -> (frame: JSON, body: Data, isBinary: Bool) {
        let r = try await inner.recv()
        keep(
            Crossed(
                dir: "out", kind: r.isBinary ? "binary" : "text", head: r.frame,
                body: r.isBinary ? r.body : nil))
        return r
    }
    func close() async { await inner.close() }
}

/// A WS transport dialing `port`'s `/web/ws`, wrapped in a tap.
private func tapWeb(_ port: UInt16) async throws -> WireTap {
    let client = try await NIOWebSocketClient.connect(
        url: URL(string: "ws://127.0.0.1:\(port)/web/ws")!)
    return WireTap(WSTransport(client: client))
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

private let NO_REST_FORM: JSON = [
    "error": "web_rest: the reply has no JSON representation on this leg",
    "reason": "transport_error",
    "hint": "this verb answers with in-process objects; call it in-process, not over HTTP",
]

@Suite("any verb's bytes reply rides the binary channel", .serialized)
struct AnyVerbBytesReplyTests {

    @Test(.timeLimit(.minutes(1)))
    func aWebWsCustomVerbTextCallGetsABinaryReplyFrame() async throws {
        let (k, port, probe) = await one()
        let ws = socket(port)
        let (head, body) = try await wsCall(ws, "e1", ["type": "events"])
        #expect(body == CHUNK, "a binary frame carrying the chunk: \(head.serialize())")
        #expect(
            head
                == [
                    "type": "reply", "id": "e1", "data": ["seq": 3, "chunk": .null],
                    "_binary_path": ["data", "chunk"],
                ], "\(head.serialize())")

        // the SAME verb with no bytes in its reply: a plain TEXT frame
        let (idle, none) = try await wsCall(ws, "e2", ["type": "idle"])
        #expect(none == nil, "a text frame: \(idle.serialize())")
        #expect(idle == ["type": "reply", "id": "e2", "data": ["seq": 3]], "\(idle.serialize())")
        ws.cancel(with: .normalClosure, reason: nil)
        #expect(probe.calls == ["events", "idle"])
        await stopWeb(k)
    }

    @Test(
        .timeLimit(.minutes(1)),
        arguments: [
            ("nested", ["seq": 3, "out": ["chunk": .null]], ["data", "out", "chunk"]),
            ("listed", ["events": [["n": 0], ["n": 1, "bytes": .null]]], ["data", "events", 1, "bytes"]),
        ] as [(String, JSON, JSON)])
    func eWebWsCarriesBytesAtAnyDepth(_ c: (String, JSON, JSON)) async throws {
        let (verb, data, path) = c
        let (k, port, _) = await one()
        let ws = socket(port)
        let (head, body) = try await wsCall(ws, "n1", ["type": .string(verb)])
        #expect(
            head == ["type": "reply", "id": "n1", "data": data, "_binary_path": path],
            "\(head.serialize())")
        #expect(body == CHUNK)
        ws.cancel(with: .normalClosure, reason: nil)
        await stopWeb(k)
    }

    @Test(.timeLimit(.minutes(1)))
    func bWebRestCustomVerbJsonCallGetsAnOctetReply() async throws {
        let (k, port, _) = await one()
        let r = try await restCall(port, "probe", "t_rest", ["type": "events"])
        #expect(r.status == 200)
        #expect(r.type == "application/octet-stream")
        #expect(r.field == "chunk")
        #expect((try? JSON.parse(r.rest ?? "")) == ["seq": 3], "\(r.rest ?? "nil")")
        #expect(r.body == CHUNK)

        // no bytes in the reply ⇒ plain JSON, no octet headers
        let j = try await restCall(port, "probe", "t_rest", ["type": "idle"])
        #expect(j.status == 200)
        #expect(j.type == "application/json")
        #expect(j.field == nil)
        #expect(j.rest == nil)
        #expect((try? JSON.parse(j.body)) == ["seq": 3])
        await stopWeb(k)
    }

    /// The octet reply names ONE top-level field (`X-Fantastic-Bytes-Field` is a
    /// literal key, never a path), so bytes below the top level cannot ride it,
    /// and JSON has no bytes: the leg answers its stated transport refusal.
    @Test(.timeLimit(.minutes(1)), arguments: ["nested", "listed"])
    func eWebRestBytesBelowTheTopLevelHaveNoRestForm(_ verb: String) async throws {
        let (k, port, probe) = await one()
        let r = try await restCall(port, "probe", "t_rest", ["type": .string(verb)])
        #expect(r.status == 500)
        #expect(r.type == "application/json")
        #expect((try? JSON.parse(r.body)) == NO_REST_FORM, "\(String(decoding: r.body, as: UTF8.self))")
        #expect(probe.calls == [verb])
        await stopWeb(k)
    }

    @Test(.timeLimit(.minutes(1)))
    func cAWsBridgeForwardTextCallBinaryReply() async throws {
        let (ka, _, aBridge) = await kernel(guid: "A-g")
        let (kb, _, _) = await kernel(guid: "B-g")
        let port = await serveWeb(
            kb,
            legs: [
                [
                    "handler_module": "web_ws.tools", "id": "b_ws",
                    "ingress_rule": "allow_all_ingress", "can_call": ["probe": "*"],
                ]
            ])
        _ = await create(
            kb,
            [
                "parent_id": "kernel_state", "handler_module": "anyverb_bytes_probe.tools",
                "id": "probe", "callable_by": ["b_ws": "*"],
            ])
        let aLeg = await create(
            ka,
            [
                "parent_id": "kernel_state", "handler_module": "ws_bridge.tools",
                "transport": "memory", "peer_id": "kernel_state",
            ])
        let sock = try await tapWeb(port)
        aBridge.injectTransport(agentId: AgentId(aLeg), transport: sock)
        let booted = await ka.send([Item(AgentId(aLeg), ["type": "boot"])])[0].result
        #expect(booted["booted"].asBool == true, "\(booted.serialize())")

        // swift's in-process caller takes the reply's body raw (`rawReply`); the
        // bytes python's result holds at `chunk` are that body, at that slot
        let out = await ka.send([
            Item(
                AgentId(aLeg),
                ["type": "forward", "target": "probe", "payload": ["type": "events"]],
                rawReply: true)
        ])[0]
        #expect(out.result == ["seq": 3, "chunk": .null], "\(out.result.serialize())")
        #expect(out.body == Blob("chunk", CHUNK), "\(String(describing: out.body))")

        let crossed = sock.callsAndReplies
        #expect(
            crossed.map { "\($0.dir) \($0.kind)" } == ["in text", "out binary"],
            "\(crossed.map { $0.head.serialize() })")
        if crossed.count == 2 {
            let call = crossed[0].head
            #expect(call["target"] == "probe" && call["payload"] == ["type": "events"])
            #expect(
                crossed[1].head
                    == [
                        "type": "reply", "id": call["id"], "data": ["seq": 3, "chunk": .null],
                        "_binary_path": ["data", "chunk"],
                    ], "\(crossed[1].head.serialize())")
            #expect(crossed[1].body == CHUNK)
        }
        _ = await ka.send([Item(AgentId("kernel"), ["type": "delete_agent", "id": .string(aLeg)])])
        await stopWeb(kb)
    }

    /// H serves web_rest + web_ws; P dials H's web_ws with a ws_bridge and says
    /// `hello`. A JSON post to H's REST reaches `P-g|probe`: H's web_ws sends the
    /// call as TEXT, P's bridge ENGINE serves it and replies BINARY, H's REST
    /// answers octet.
    @Test(.timeLimit(.minutes(1)))
    func dAPeerBridgeEngineAnswersATextCallWithBytes() async throws {
        let (kh, _, _) = await kernel(guid: "A-g")
        let (kp, _, pBridge) = await kernel(guid: "B-g")
        let port = await serveWeb(
            kh,
            legs: [
                ["handler_module": "web_ws.tools", "id": "h_ws", "ingress_rule": "allow_all_ingress"],
                [
                    "handler_module": "web_rest.tools", "id": "h_rest",
                    "ingress_rule": "allow_all_ingress", "can_call": ["P-g|probe": "*"],
                ],
            ])
        let pLeg = await create(
            kp,
            [
                "parent_id": "kernel_state", "handler_module": "ws_bridge.tools",
                "transport": "memory", "peer_id": "kernel_state",
                "ingress_rule": "allow_all_ingress", "can_call": ["probe": "*"],
            ])
        // what the serving leg relays is its own call: the probe accepts the leg
        _ = await create(
            kp,
            [
                "parent_id": "kernel_state", "handler_module": "anyverb_bytes_probe.tools",
                "id": "probe", "callable_by": .object([pLeg: "*"]),
            ])
        let sock = try await tapWeb(port)
        try await sock.send(["type": "hello", "guid": "P-g"])
        pBridge.injectTransport(agentId: AgentId(pLeg), transport: sock)
        let booted = await kp.send([Item(AgentId(pLeg), ["type": "boot"])])[0].result
        #expect(booted["booted"].asBool == true, "\(booted.serialize())")
        try await until("P-g connected") { !kh.peerRoutes("P-g").isEmpty }

        let r = try await restCall(port, "P-g%7Cprobe", "h_rest", ["type": "events"])
        #expect(r.status == 200, "\(String(decoding: r.body, as: UTF8.self))")
        #expect(r.type == "application/octet-stream")
        #expect(r.field == "chunk")
        #expect((try? JSON.parse(r.rest ?? "")) == ["seq": 3], "\(r.rest ?? "nil")")
        #expect(r.body == CHUNK)

        let crossed = sock.callsAndReplies
        #expect(
            crossed.map { "\($0.dir) \($0.kind)" } == ["out text", "in binary"],
            "\(crossed.map { $0.head.serialize() })")
        if crossed.count == 2 {
            let call = crossed[0].head
            #expect(call["target"] == "probe" && call["payload"] == ["type": "events"])
            #expect(
                crossed[1].head
                    == [
                        "type": "reply", "id": call["id"], "data": ["seq": 3, "chunk": .null],
                        "_binary_path": ["data", "chunk"],
                    ], "\(crossed[1].head.serialize())")
            #expect(crossed[1].body == CHUNK)
        }
        _ = await kp.send([Item(AgentId("kernel"), ["type": "delete_agent", "id": .string(pLeg)])])
        await stopWeb(kh)
    }
}
