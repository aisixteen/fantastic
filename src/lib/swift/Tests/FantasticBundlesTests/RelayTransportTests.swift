// relay_connector transport — in-process symmetric round-trip over a memory hub.
//
// The Swift analog of Rust's relay-transport tests + Python's FakeRelayHub: two
// `RelayTransport` legs are wired to a `MemoryRelayHub` that emulates the
// relay-kernel's routing (a `send` to a target GUID is re-emitted to that peer as
// `{type:"event", source, payload}`, preserving the WS frame kind — text or raw
// binary). Exercises the envelope wrap/unwrap, symmetric inbound `call` dispatch,
// raw-byte streaming (no base64), the ingress gate (deny/password), AND a >64 KiB
// frame — all without a network. The live-relay path is `integration_tests/relay_e2e`.

import FantasticFile
import FantasticIoBridge
import FantasticJSON
import FantasticKernel
import FantasticKernelBridge
import Foundation
import Testing

// ── kernels ────────────────────────────────────────────────────

private final class EchoBundle: AgentBundle {
    let name = "echo"
    func handle(agentId: AgentId, payload: JSON, kernel: Kernel) async throws -> JSON? {
        .object(["echoed": payload])
    }
}

private func makeKernel(withEcho: Bool) async -> Kernel {
    let registry = BundleRegistry()
    if withEcho { registry.register("echo.tools", EchoBundle()) }
    let kernel = Kernel(storage: .inMemory, bundles: registry)
    let root = Agent(id: "core", handlerModule: nil, parentId: nil)
    kernel.register(root)
    kernel.setRoot(root)
    if withEcho {
        _ = await kernel.send(
            "core",
            ["type": "create_agent", "handler_module": "echo.tools", "id": "echo"])
    }
    return kernel
}

private struct TestDeadlineExceeded: Error { let seconds: UInt64 }

private func withDeadline<T: Sendable>(
    _ seconds: UInt64, _ body: @escaping @Sendable () async throws -> T
) async throws -> T {
    try await withThrowingTaskGroup(of: T.self) { group in
        group.addTask { try await body() }
        group.addTask {
            try await Task.sleep(nanoseconds: seconds * 1_000_000_000)
            throw TestDeadlineExceeded(seconds: seconds)
        }
        let r = try await group.next()!
        group.cancelAll()
        return r
    }
}

// ── in-process relay emulator ──────────────────────────────────

/// One peer's WS surface against the hub. `send` hands a relay envelope to the
/// hub for routing; `receive` pops the peer's delivered events.
private actor MemoryRelayWire: RelayWire {
    let guid: String
    private let hub: MemoryRelayHub
    private var inbox: [NIOWebSocketClient.Message] = []
    private var waiters: [CheckedContinuation<NIOWebSocketClient.Message, Error>] = []
    private var closed = false

    init(guid: String, hub: MemoryRelayHub) {
        self.guid = guid
        self.hub = hub
    }

    func send(_ message: NIOWebSocketClient.Message) async throws {
        await hub.route(from: guid, message)
    }

    func receive() async throws -> NIOWebSocketClient.Message {
        if !inbox.isEmpty { return inbox.removeFirst() }
        if closed { throw NIOWebSocketClient.Closed(reason: "memory wire closed") }
        return try await withCheckedThrowingContinuation { cont in waiters.append(cont) }
    }

    func deliver(_ m: NIOWebSocketClient.Message) {
        if !waiters.isEmpty {
            waiters.removeFirst().resume(returning: m)
        } else {
            inbox.append(m)
        }
    }

    func close() async {
        closed = true
        let w = waiters
        waiters.removeAll()
        for c in w { c.resume(throwing: NIOWebSocketClient.Closed(reason: "memory wire closed")) }
    }
}

/// Emulates the relay kernel: a `send` to a target GUID is re-emitted to that
/// peer's socket as a `{type:"event", source, payload}` frame, in the SAME WS
/// frame kind (text or native binary — raw bytes, no base64).
private actor MemoryRelayHub {
    private var wires: [String: MemoryRelayWire] = [:]
    /// Per-guid advertised directory attrs (set by an `announce` frame).
    private var announcedAttrs: [String: JSON] = [:]

    func register(_ guid: String) -> MemoryRelayWire {
        let w = MemoryRelayWire(guid: guid, hub: self)
        wires[guid] = w
        return w
    }

    func announced(of guid: String) -> JSON? { announcedAttrs[guid] }

    func route(from sender: String, _ message: NIOWebSocketClient.Message) async {
        switch message {
        case .text(let s):
            guard let env = try? JSON.parse(s) else { return }
            switch env["type"].asString {
            case "announce":
                // Directory typing: store the opaque attrs blob (the relay never
                // interprets it). A real relay would also emit `peer_updated`.
                announcedAttrs[sender] = env["attrs"]
                return
            case "send":
                break  // fall through to routing below
            default:
                return
            }
            guard let target = env["target"].asString else { return }
            let event: JSON = .object([
                "type": .string("event"), "source": .string(sender), "payload": env["payload"],
            ])
            await wires[target]?.deliver(.text(event.serialize()))
        case .binary(let bytes):
            guard let (env, body) = Codec.decodeBinaryFrame(Data(bytes)),
                env["type"].asString == "send", let target = env["target"].asString
            else { return }
            var event: JSON = .object([
                "type": .string("event"), "source": .string(sender), "payload": env["payload"],
            ])
            if let p = env["_binary_path"].asString { event["_binary_path"] = .string(p) }
            let wire = Codec.encodeBinaryFrame(header: event, body: body)
            await wires[target]?.deliver(.binary([UInt8](wire)))
        }
    }
}

/// Wire two relay legs (A↔B) through a fresh hub, each with its own kernel + rules.
private func pairTransports(
    kernelA: Kernel, kernelB: Kernel,
    ingressA: IngressRule = IngressRules.AllowAll(), egressA: EgressRule = EgressRules.Silent(),
    ingressB: IngressRule = IngressRules.AllowAll(), egressB: EgressRule = EgressRules.Silent()
) async -> (RelayTransport, RelayTransport) {
    let hub = MemoryRelayHub()
    let wireA = await hub.register("A")
    let wireB = await hub.register("B")
    let ta = await RelayTransport.attach(
        wire: wireA, partnerGuid: "B", localAgentId: "brA", localKernel: kernelA,
        ingress: ingressA, egress: egressA)
    let tb = await RelayTransport.attach(
        wire: wireB, partnerGuid: "A", localAgentId: "brB", localKernel: kernelB,
        ingress: ingressB, egress: egressB)
    return (ta, tb)
}

// ── transport tests ────────────────────────────────────────────

@Suite("relay_connector transport")
struct RelayTransportTests {
    @Test func forwardRoundTripsOverRelay() async throws {
        try await withDeadline(15) {
            let kernelA = await makeKernel(withEcho: false)
            let kernelB = await makeKernel(withEcho: true)
            let (transportA, transportB) = await pairTransports(kernelA: kernelA, kernelB: kernelB)

            // 1) forward A → B's "echo" agent → reply tunnels back.
            let r1 = await transportA.forward(
                target: "echo", payload: ["type": "echo", "msg": "hi"])
            #expect(r1["echoed"]["msg"].asString == "hi")

            // 2) >64 KiB frame survives.
            let big = String(repeating: "x", count: 200_000)
            let r2 = await transportA.forward(
                target: "echo", payload: ["type": "echo", "blob": .string(big)])
            #expect(r2["echoed"]["blob"].asString?.count == big.count)

            // 3) reverse: B → A's "core" (both legs serve inbound calls — symmetric).
            let r3 = await transportB.forward(target: "core", payload: ["type": "list_agents"])
            let names = (r3["agents"].asArray ?? []).compactMap { $0["id"].asString }
            #expect(names.contains("core"))

            await transportA.close()
            await transportB.close()
        }
    }

    @Test func binaryForwardStreamsRawBytesOverRelay() async throws {
        try await withDeadline(20) {
            let kernelA = await makeKernel(withEcho: false)
            // kernelB hosts an OPEN file_bridge in a cwd-relative dir.
            let registry = BundleRegistry()
            registry.register("file_bridge.tools", FileBundle())
            let kernelB = Kernel(storage: .inMemory, bundles: registry)
            let rootB = Agent(id: "core", handlerModule: nil, parentId: nil)
            kernelB.register(rootB)
            kernelB.setRoot(rootB)
            let fm = FileManager.default
            let rel = "fantastic-relaystream-\(UUID().uuidString)"
            let dir = URL(fileURLWithPath: fm.currentDirectoryPath).appendingPathComponent(rel)
            try fm.createDirectory(at: dir, withIntermediateDirectories: true)
            defer { try? fm.removeItem(at: dir) }
            _ = await kernelB.send(
                "core",
                [
                    "type": "create_agent", "handler_module": "file_bridge.tools",
                    "id": "fs", "root": .string(rel), "ingress_rule": "allow_all",
                ])

            let (transportA, transportB) = await pairTransports(kernelA: kernelA, kernelB: kernelB)

            let payload = Data([0x00, 0xFF, 0xCA, 0xFE, 0xBA, 0xBE, 0x10, 0x80])
            // write_stream (binary) over the relay tunnel → kernelB.fs
            let (w, _) = await transportA.binaryForward(
                target: "fs",
                header: ["type": "write_stream", "path": "blob.bin", "truncate": .bool(true)],
                blob: payload)
            #expect(w["written"].asInt == Int64(payload.count), "\(w)")
            // read_stream (binary) over the relay tunnel → raw bytes back
            let (_, body) = await transportA.binaryForward(
                target: "fs", header: ["type": "read_stream", "path": "blob.bin"], blob: Data())
            #expect(body == payload, "raw bytes must round-trip over the relay tunnel")

            await transportA.close()
            await transportB.close()
        }
    }

    @Test func setIdentityAdvertisesDirectoryAttrs() async throws {
        try await withDeadline(15) {
            let hub = MemoryRelayHub()
            let wire = await hub.register("mgr")
            // A plain attach advertises nothing (empty identity).
            let t = await RelayTransport.attach(wire: wire, partnerGuid: "B")
            #expect(await hub.announced(of: "mgr") == nil)
            // set_identity advertises the opaque attrs blob to the relay.
            _ = await t.setIdentity(["role": .string("manager"), "exposes": [.string("stop")]])
            let attrs = await hub.announced(of: "mgr")
            #expect(attrs?["role"].asString == "manager")
            #expect(attrs?["exposes"].asArray?.count == 1)
            await t.close()
        }
    }

    @Test func denyInboundRefusesReverseCall() async throws {
        try await withDeadline(15) {
            let kernelA = await makeKernel(withEcho: false)
            let kernelB = await makeKernel(withEcho: true)
            // B's leg is sealed — its inbound-call gate denies.
            let (transportA, transportB) = await pairTransports(
                kernelA: kernelA, kernelB: kernelB, ingressB: IngressRules.DenyInbound())
            let r = await transportA.forward(target: "echo", payload: ["type": "echo"])
            #expect(r["reason"].asString == "unauthorized", "\(r)")
            await transportA.close()
            await transportB.close()
        }
    }
}

// ── password auth over the relay tunnel ────────────────────────

@Suite("relay_connector password auth", .serialized)
struct RelayPasswordTests {
    @Test func passwordGroupMemberRoundTrips() async throws {
        let env = "FANTASTIC_GROUP_TOKEN_RELAY_UNIT"
        setenv(env, "s3cret", 1)
        defer { unsetenv(env) }
        let rule: JSON = ["type": "password", "env": .string(env)]
        try await withDeadline(15) {
            let kernelA = await makeKernel(withEcho: false)
            let kernelB = await makeKernel(withEcho: true)
            // A presents the group token (egress); B checks it (ingress). Both pass.
            let (transportA, transportB) = await pairTransports(
                kernelA: kernelA, kernelB: kernelB,
                egressA: try EgressRules.resolve(rule),
                ingressB: try IngressRules.resolve(rule))
            let r = await transportA.forward(
                target: "echo", payload: ["type": "echo", "msg": "hey"])
            #expect(r["echoed"]["msg"].asString == "hey", "\(r)")
            await transportA.close()
            await transportB.close()
        }
    }

    @Test func passwordRejectsTokenlessCaller() async throws {
        let env = "FANTASTIC_GROUP_TOKEN_RELAY_UNIT2"
        setenv(env, "s3cret", 1)
        defer { unsetenv(env) }
        try await withDeadline(15) {
            let kernelA = await makeKernel(withEcho: false)
            let kernelB = await makeKernel(withEcho: true)
            // A presents NOTHING (silent egress); B requires the token → deny.
            let (transportA, transportB) = await pairTransports(
                kernelA: kernelA, kernelB: kernelB,
                ingressB: try IngressRules.resolve(["type": "password", "env": .string(env)]))
            let r = await transportA.forward(target: "echo", payload: ["type": "echo"])
            #expect(r["reason"].asString == "unauthorized", "\(r)")
            await transportA.close()
            await transportB.close()
        }
    }
}
