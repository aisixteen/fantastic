// A RAW BODY reaches its handler at the field python puts it under — on every
// leg. Python's web_ws places a binary frame's body at the header's
// `_binary_path`, web_rest at `X-Fantastic-Bytes-Field`, and each handler reads
// ITS OWN key: `exec_bridge.paste_image` reads `data`, `file_bridge.write_stream`
// reads `bytes`. The kernel's binary channel carries the body WITH that path
// (`Blob`), so `paste_image` works over the binary channel exactly as it does in
// python, and a body under a foreign key is not its body.
//
// Twin of rust `fantastic-web/src/tests_binary_path.rs`, over a real loopback
// socket.

import FantasticExecBridge
import FantasticIoBridge
import FantasticJSON
import FantasticKernel
import FantasticKernelBridge
import FantasticWeb
import FantasticWebRest
import FantasticWebWS
import Foundation
import OrderedCollections
import Testing

/// Answers what reached it: the body's path and length, and the header the
/// handler was given (python's payload minus the bytes).
private final class Capture: AgentBundle, @unchecked Sendable {
    let name = "capture"
    func handle(agentId: AgentId, payload: JSON, kernel: Kernel) async throws -> JSON? {
        switch payload["type"].asString ?? "" {
        case "boot", "shutdown": return nil
        default: return .object(["channel": "text", "header": payload])
        }
    }
    func handleBinary(agentId: AgentId, header: JSON, body: Blob?, kernel: Kernel) async throws
        -> (JSON?, Data?)
    {
        (
            .object([
                "channel": "binary",
                "path": body.map { Codec.binaryPath($0.parts) } ?? .null,
                "len": body.map { JSON.integer(Int64($0.bytes.count)) } ?? .null,
                "header": header,
            ]), nil
        )
    }
}

/// Python's payload as its handler holds it: the header with the body put back
/// at its slot (decoded), and the top-level keys sorted — the twin of python's
/// `rest_bytes_echo` (`{keys: sorted(payload), values: {k: bytes→str}}`).
private final class PyView: AgentBundle, @unchecked Sendable {
    let name = "pyview"
    func handle(agentId: AgentId, payload: JSON, kernel: Kernel) async throws -> JSON? {
        switch payload["type"].asString ?? "" {
        case "boot", "shutdown": return nil
        default: return Self.view(payload)
        }
    }
    func handleBinary(agentId: AgentId, header: JSON, body: Blob?, kernel: Kernel) async throws
        -> (JSON?, Data?)
    {
        var whole = header
        if let body {
            let text = String(decoding: body.bytes, as: UTF8.self)
            try pySetPath(&whole, parts: body.parts, .string(text))
        }
        return (Self.view(whole), nil)
    }
    static func view(_ payload: JSON) -> JSON {
        let keys = (payload.asObject.map { Array($0.keys) } ?? []).sorted()
        return .object(["keys": .array(keys.map { .string($0) }), "values": payload])
    }
}

/// The far kernel's probe for a bridge forward (python `_echo` in
/// `test_binary_path_steps.py`): answers the payload's keys — the body's own
/// top-level key among them — where the body sat and the nested `a`, and hands
/// the body back as the reply's chunk.
private final class BytesEcho: AgentBundle, @unchecked Sendable {
    let name = "bytes_echo"
    func handle(agentId: AgentId, payload: JSON, kernel: Kernel) async throws -> JSON? {
        switch payload["type"].asString ?? "" {
        case "boot", "shutdown": return nil
        default: return .object(["channel": "text"])
        }
    }
    func handleBinary(agentId: AgentId, header: JSON, body: Blob?, kernel: Kernel) async throws
        -> (JSON?, Data?)
    {
        var keys = header.asObject.map { Array($0.keys) } ?? []
        if let body, body.parts.count == 1, case .key(let k) = body.parts[0] { keys.append(k) }
        return (
            .object([
                "keys": .array(keys.sorted().map { .string($0) }),
                "at": body.map { Codec.binaryPath($0.parts) } ?? .null,
                "a": header["a"],
            ]), body?.bytes
        )
    }
}

/// A ws_bridge leg's socket, keeping the head of every BINARY frame that crosses
/// it, both ways — the test's capture of the wire (python `_WsEnd.heads`), around
/// the real client.
private actor RecordingWire: RelayWire {
    let inner: NIOWebSocketClient
    private(set) var heads: [JSON] = []
    init(_ inner: NIOWebSocketClient) { self.inner = inner }
    private func keep(_ m: NIOWebSocketClient.Message) {
        if case .binary(let b) = m, let (h, _) = try? Codec.decodeBinaryFrame(Data(b)) {
            heads.append(h)
        }
    }
    func send(_ message: NIOWebSocketClient.Message) async throws {
        keep(message)
        try await inner.send(message)
    }
    func receive() async throws -> NIOWebSocketClient.Message {
        let m = try await inner.receive()
        keep(m)
        return m
    }
    func close() async { await inner.close() }
}

/// `web` + its ws and rest legs (`ws_leg`, `rest_leg`, open), an exec_bridge
/// `term`, a `cap`ture agent, a python-`view` agent and a bytes `echo` both legs
/// may call.
/// Returns the bound port.
private func stage(_ dir: URL) async -> (Kernel, UInt16) {
    let r = BundleRegistry()
    r.register("web.tools", WebBundle())
    r.register("web_ws.tools", WebWSBundle())
    r.register("web_rest.tools", WebRestBundle())
    r.register("exec_bridge.tools", ExecBridgeBundle())
    r.register("capture.tools", Capture())
    r.register("pyview.tools", PyView())
    r.register("bytes_echo.tools", BytesEcho())
    let k = Kernel(storage: .inMemory, bundles: r)
    let root = Agent(id: AgentId("kernel_state"), handlerModule: nil, parentId: nil)
    k.register(root)
    k.setRoot(root)
    let callers: JSON = ["ws_leg": "*", "rest_leg": "*"]
    let legGrant: JSON = ["term": "*", "cap": "*", "view": "*", "echo": "*"]
    for p: JSON in [
        [
            "type": "create_agent", "parent_id": "kernel_state", "handler_module": "web.tools",
            "id": "web", "port": .integer(Int64(freeTCPPort())),
        ],
        [
            "type": "create_agent", "parent_id": "web", "handler_module": "web_ws.tools",
            "id": "ws_leg", "ingress_rule": "allow_all_ingress", "can_call": legGrant,
        ],
        [
            "type": "create_agent", "parent_id": "web", "handler_module": "web_rest.tools",
            "id": "rest_leg", "ingress_rule": "allow_all_ingress", "can_call": legGrant,
        ],
        [
            "type": "create_agent", "parent_id": "kernel_state",
            "handler_module": "exec_bridge.tools", "id": "term",
            "ingress_rule": "allow_all_ingress", "cwd": .string(dir.path), "allowed": [".*"],
            "callable_by": callers,
        ],
        [
            "type": "create_agent", "parent_id": "kernel_state", "handler_module": "capture.tools",
            "id": "cap", "callable_by": callers,
        ],
        [
            "type": "create_agent", "parent_id": "kernel_state", "handler_module": "pyview.tools",
            "id": "view", "callable_by": callers,
        ],
        [
            "type": "create_agent", "parent_id": "kernel_state",
            "handler_module": "bytes_echo.tools", "id": "echo", "callable_by": callers,
        ],
    ] {
        let made = await k.send([Item(AgentId("kernel"), p)])[0].result
        #expect(made["error"].isNull, "stage: \(made.serialize())")
    }
    for leg in ["ws_leg", "rest_leg"] {
        _ = await k.send([
            Item(AgentId("web"), .object(["type": "mount", "child_id": .string(leg)]))
        ])
    }
    return (k, k.httpPort())
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

/// A binary `call` frame: `payload` with `null` at `at` (python's head), the body
/// at `_binary_path: ["payload", …at]`.
private func callFrame(
    _ id: String, _ target: String, _ payload: JSON, _ at: [PathStep], _ body: Data
) throws -> URLSessionWebSocketTask.Message {
    var p = payload
    try pySetPath(&p, parts: at, .null)
    let head: JSON = .object([
        "type": "call", "id": .string(id), "target": .string(target), "payload": p,
        "_binary_path": Codec.payloadPath(at),
    ])
    return .data(Codec.encodeBinaryFrame(header: head, body: body))
}

/// The next frame off the socket, BOUNDED: past 5 s the socket is cancelled.
private func nextFrame(_ task: URLSessionWebSocketTask) async throws -> JSON {
    let timer = Task {
        try await Task.sleep(nanoseconds: 5_000_000_000)
        task.cancel(with: .goingAway, reason: nil)
    }
    defer { timer.cancel() }
    while true {
        switch try await task.receive() {
        case .string(let s): return try JSON.parse(s)
        case .data(let d): return try Codec.decodeBinaryFrame(d).header
        @unknown default: continue
        }
    }
}

/// The `reply` frame answering `id`, BOUNDED: past 5 s the socket is cancelled,
/// which makes the pending receive throw.
private func replyOf(_ task: URLSessionWebSocketTask, _ id: String) async throws -> JSON {
    let timer = Task {
        try await Task.sleep(nanoseconds: 5_000_000_000)
        task.cancel(with: .goingAway, reason: nil)
    }
    defer { timer.cancel() }
    while true {
        let frame: JSON
        switch try await task.receive() {
        case .string(let s): frame = try JSON.parse(s)
        case .data(let d): frame = try Codec.decodeBinaryFrame(d).header
        @unknown default: continue
        }
        if frame["type"].asString == "reply", frame["id"].asString == id { return frame["data"] }
    }
}

/// An octet POST to `rest_leg/<target>`: the body bound under `field` (no
/// `X-Fantastic-Bytes-Field` header at all when `nil`).
private func octet(
    _ port: UInt16, _ target: String, _ payload: String, field: String?, _ body: Data
) async throws -> (Int, JSON) {
    var req = URLRequest(url: URL(string: "http://127.0.0.1:\(port)/rest_leg/\(target)")!)
    req.httpMethod = "POST"
    req.setValue("application/octet-stream", forHTTPHeaderField: "Content-Type")
    req.setValue(payload, forHTTPHeaderField: "X-Fantastic-Payload")
    if let field { req.setValue(field, forHTTPHeaderField: "X-Fantastic-Bytes-Field") }
    req.httpBody = body
    let (d, resp) = try await URLSession(configuration: .ephemeral).data(for: req)
    return ((resp as? HTTPURLResponse)?.statusCode ?? 0, (try? JSON.parse(d)) ?? .null)
}

private let RAW = Data([0x00]) + Data("raw".utf8) + Data([0xFF])

private func tmpDir() -> URL {
    let d = FileManager.default.temporaryDirectory
        .appendingPathComponent("binpath-\(UUID().uuidString.prefix(8))")
    try? FileManager.default.createDirectory(at: d, withIntermediateDirectories: true)
    return d
}

@Suite("a raw body at its path — every leg", .serialized)
struct BinaryBodyPathTests {

    /// `paste_image` over the BINARY channel, from both wire legs: the image is
    /// the body at `data` — a WS frame's `_binary_path: payload.data`, a REST
    /// call's `X-Fantastic-Bytes-Field: data` — and lands in the PTY job's scratch
    /// file. A body under `bytes` is not `data`: python's `paste_image` answers
    /// `bad_arg data "raw bytes"`, and so does this one.
    @Test(.timeLimit(.minutes(1)))
    func pasteImageReadsItsBodyAtDataOverWsAndRest() async throws {
        let dir = tmpDir()
        defer { try? FileManager.default.removeItem(at: dir) }
        let (k, port) = await stage(dir)
        let started = await k.send([
            Item(
                AgentId("term"),
                .object(["type": "start", "command": "/bin/cat", "pty": ["cols": 80, "rows": 24]]))
        ])[0].result
        #expect(started["pty"] == true, "\(started.serialize())")
        let job = started["job_id"]
        let ws = socket(port)
        defer { ws.cancel(with: .normalClosure, reason: nil) }
        let png = Data([0x89] + Array("PNG\r\n\u{1a}\n-ws-".utf8))

        try await ws.send(
            callFrame("p1", "term", ["type": "paste_image", "job_id": job, "mime": "image/png"], ["data"], png))
        let r = try await replyOf(ws, "p1")
        #expect(r["bytes"] == .integer(Int64(png.count)), "\(r.serialize())")
        let saved = try #require(r["path"].asString, "a saved path: \(r.serialize())")
        #expect(saved.hasSuffix(".png"), "\(r.serialize())")
        #expect(FileManager.default.contents(atPath: saved) == png, "the body IS the image")

        // the same body under `bytes` is not the image
        try await ws.send(
            callFrame("p2", "term", ["type": "paste_image", "job_id": job, "mime": "image/png"], ["bytes"], png))
        let refused = try await replyOf(ws, "p2")
        #expect(
            refused == Refusal.bad("paste_image", "data", "raw bytes"), "\(refused.serialize())")

        // REST: the octet body bound under the NAMED field
        let restPng = Data([0x89] + Array("PNG\r\n\u{1a}\n-rest-".utf8))
        let (status, rr) = try await octet(
            port, "term",
            JSON.object(["type": "paste_image", "job_id": job, "mime": "image/png"]).serialize(),
            field: "data", restPng)
        #expect(status == 200, "\(rr.serialize())")
        #expect(rr["bytes"] == .integer(Int64(restPng.count)), "\(rr.serialize())")
        #expect(FileManager.default.contents(atPath: rr["path"].asString ?? "") == restPng)

        _ = await k.send([Item(AgentId("term"), .object(["type": "stop", "job_id": job]))])
        await stopWeb(k)
    }

    /// ANY path under the frame's `payload` — nested included — reaches the
    /// handler as the body's path, and the header it gets holds nothing at that
    /// slot (python holds the bytes there). REST binds the field literally.
    @Test(.timeLimit(.minutes(1)))
    func aBodyReachesItsHandlerAtThePathTheWireNamed() async throws {
        let dir = tmpDir()
        defer { try? FileManager.default.removeItem(at: dir) }
        let (k, port) = await stage(dir)
        let ws = socket(port)
        defer { ws.cancel(with: .normalClosure, reason: nil) }
        try await ws.send(
            callFrame(
                "n1", "cap", ["type": "take", "meta": ["k": 1]], ["meta", "blob"], Data("abc".utf8)))
        #expect(
            try await replyOf(ws, "n1")
                == [
                    "channel": "binary", "path": ["meta", "blob"], "len": 3,
                    "header": ["type": "take", "meta": ["k": 1]],
                ])
        // a LIST slot, by index
        try await ws.send(
            callFrame(
                "n2", "cap", ["type": "take", "items": [["n": 0], ["data": .null]]],
                ["items", 1, "data"], Data("ab".utf8)))
        #expect(
            try await replyOf(ws, "n2")
                == [
                    "channel": "binary", "path": ["items", 1, "data"], "len": 2,
                    "header": ["type": "take", "items": [["n": 0], [:]]],
                ])
        let (status, r) = try await octet(
            port, "cap", #"{"type": "take", "k": 2}"#, field: "data", Data("wxyz".utf8))
        #expect(status == 200, "\(r.serialize())")
        #expect(
            r == ["channel": "binary", "path": ["data"], "len": 4, "header": ["type": "take", "k": 2]],
            "\(r.serialize())")
        await stopWeb(k)
    }

    /// `X-Fantastic-Bytes-Field: a.b` binds the body to the key `"a.b"` — never a
    /// dotted path into the envelope: a nested `a.b` already in
    /// X-Fantastic-Payload is left as sent. An absent header binds `bytes`.
    /// Python `test_the_bytes_field_header_names_a_literal_top_level_key`.
    @Test(.timeLimit(.minutes(1)))
    func theBytesFieldHeaderNamesALiteralTopLevelKey() async throws {
        let dir = tmpDir()
        defer { try? FileManager.default.removeItem(at: dir) }
        let (k, port) = await stage(dir)
        let (status, r) = try await octet(
            port, "view", #"{"type": "take", "a": {"b": "kept"}}"#, field: "a.b", Data("raw".utf8))
        #expect(status == 200, "\(r.serialize())")
        #expect(
            r == [
                "keys": ["a", "a.b", "type"],
                "values": ["type": "take", "a": ["b": "kept"], "a.b": "raw"],
            ],
            "\(r.serialize())")
        // the call row records the body's length at the literal key, the nested
        // `a.b` as sent
        let row = try #require(
            k.rows.all().last { $0["verb"]?.asString == "take" && $0["agent"]?.asString == "view" },
            "the call row")
        #expect(
            (row["payload"] ?? .null).serialize()
                == #"{"type":"take","a":{"b":"kept"},"a.b":{"len":3}}"#,
            "\((row["payload"] ?? .null).serialize())")
        let (bare, d) = try await octet(
            port, "view", #"{"type": "take"}"#, field: nil, Data("raw".utf8))
        #expect(bare == 200, "\(d.serialize())")
        #expect(
            d == ["keys": ["bytes", "type"], "values": ["type": "take", "bytes": "raw"]],
            "\(d.serialize())")
        await stopWeb(k)
    }

    /// A WS binary frame's `_binary_path` is a LIST of steps: `["payload", "a.b"]`
    /// is the literal key `"a.b"`, beside a nested `a` → `b` left as sent — the
    /// slot web_rest's `X-Fantastic-Bytes-Field: a.b` binds. The old dotted string
    /// is refused as a malformed frame, never read as a path. Python
    /// `test_binary_path_steps.py` (a).
    @Test(.timeLimit(.minutes(1)))
    func aWsFramesLiteralDottedKeyStaysLiteral() async throws {
        let dir = tmpDir()
        defer { try? FileManager.default.removeItem(at: dir) }
        let (k, port) = await stage(dir)
        let ws = socket(port)
        defer { ws.cancel(with: .normalClosure, reason: nil) }
        try await ws.send(
            callFrame(
                "l1", "view", ["type": "take", "a": ["b": "kept"]], ["a.b"], Data("raw".utf8)))
        let r = try await replyOf(ws, "l1")
        #expect(
            r == [
                "keys": ["a", "a.b", "type"],
                "values": ["type": "take", "a": ["b": "kept"], "a.b": "raw"],
            ],
            "\(r.serialize())")
        // the OLD dotted spelling: a malformed frame, answered and the socket kept
        let old: JSON = [
            "type": "call", "id": "l2", "target": "view",
            "payload": ["type": "take", "a.b": .null], "_binary_path": "payload.a.b",
        ]
        try await ws.send(.data(Codec.encodeBinaryFrame(header: old, body: Data("raw".utf8))))
        let err = try await nextFrame(ws)
        #expect(err["type"] == "error" && err["arg"] == "frame", "\(err.serialize())")
        try await ws.send(
            callFrame("l3", "view", ["type": "take"], ["0"], Data("z".utf8)))
        #expect(
            try await replyOf(ws, "l3")
                == ["keys": ["0", "type"], "values": ["type": "take", "0": "z"]])
        await stopWeb(k)
    }

    /// A literal key crosses a ws_bridge `forward` into a web_ws leg: the bridge
    /// sends the body at `["payload", "a.b"]`, the far echo finds it at the key
    /// `"a.b"` beside a nested `a` → `b` left as sent, and its chunk comes back
    /// on a binary reply. Python
    /// `test_a_literal_key_crosses_a_ws_bridge_forward_both_ways` (b). Swift's
    /// reply carrier holds a chunk at the reply's `bytes` only, so the chunk comes
    /// back at `["data", "bytes"]`, where python's echo puts it at `["data", "x.y"]`.
    @Test(.timeLimit(.minutes(1)))
    func aLiteralKeyCrossesAWsBridgeForwardIntoWebWs() async throws {
        let dir = tmpDir()
        defer { try? FileManager.default.removeItem(at: dir) }
        let (kb, port) = await stage(dir)
        // kernel A: a ws_bridge leg whose socket is B's web_ws, recorded
        let ra = BundleRegistry()
        let bridge = KernelBridgeBundle(family: .ws)
        ra.register("ws_bridge.tools", bridge)
        let ka = Kernel(storage: .inMemory, bundles: ra)
        let root = Agent(id: AgentId("kernel_state"), handlerModule: nil, parentId: nil)
        ka.register(root)
        ka.setRoot(root)
        let url = URL(string: "ws://127.0.0.1:\(port)/web/ws")!
        let wire = RecordingWire(try await NIOWebSocketClient.connect(url: url))
        let made = await ka.send([
            Item(
                AgentId("kernel"),
                [
                    "type": "create_agent", "parent_id": "kernel_state",
                    "handler_module": "ws_bridge.tools", "id": "a_leg", "transport": "memory",
                ])
        ])[0].result
        #expect(made["id"] == "a_leg", "\(made.serialize())")
        bridge.injectTransport(agentId: AgentId("a_leg"), transport: WSTransport(client: wire))
        let booted = await ka.send([Item(AgentId("a_leg"), ["type": "boot"])])[0].result
        #expect(booted["booted"] == true, "\(booted.serialize())")
        let out = await ka.send([
            Item(
                AgentId("a_leg"),
                [
                    "type": "forward", "target": "echo",
                    "payload": ["type": "take", "a": ["b": "kept"]],
                ], binary: Blob(parts: ["payload", "a.b"], RAW))
        ])[0]
        #expect(out.status == .completed, "\(out.result.serialize())")
        #expect(
            out.result == ["keys": ["a", "a.b", "type"], "at": ["a.b"], "a": ["b": "kept"]],
            "\(out.result.serialize())")
        #expect(out.body == RAW)
        let heads = await wire.heads
        #expect(
            heads.map { $0["_binary_path"] } == [["payload", "a.b"], ["data", "bytes"]],
            "\(JSON.array(heads).serialize())")
        // the null sits at the literal key; the nested twin is untouched
        #expect(
            heads.first?["payload"] == ["type": "take", "a": ["b": "kept"], "a.b": .null],
            "\(JSON.array(heads).serialize())")
        _ = await ka.send([
            Item(AgentId("kernel"), .object(["type": "delete_agent", "id": "a_leg"]))
        ])
        await stopWeb(kb)
    }
}
