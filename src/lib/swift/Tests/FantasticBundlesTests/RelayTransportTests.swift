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

private func makeKernel(withEcho: Bool, leg: String? = nil) async -> Kernel {
    let registry = BundleRegistry()
    if withEcho { registry.register("echo.tools", EchoBundle()) }
    registry.register("relay_connector.tools", KernelBridgeBundle())
    let kernel = Kernel(storage: .inMemory, bundles: registry)
    let root = Agent(id: "core", handlerModule: nil, parentId: nil)
    kernel.register(root)
    kernel.setRoot(root)
    if withEcho {
        _ = await kernel.send([Item("core", ["type": "create_agent", "parent_id": "core", "handler_module": "echo.tools", "id": "echo"])])[0].result
    }
    if let leg { await makeLeg(kernel, id: leg, canCall: withEcho ? ["echo"] : ["core"]) }
    return kernel
}

/// Create the relay leg's OWN agent record, with the grants for wherever its
/// inbound frames land.
///
/// An inbound frame now dispatches AS THE LEG — `RelayTransport.dispatchCall`
/// wraps `kernel.send` in the leg's `currentSender` — so the leg has to BE an
/// agent and hold real grants, exactly as in production. Before, the frame
/// reached the kernel with a nil sender and ran as the OPERATOR: the two-sided
/// gate never ran and a peer with a passing ingress rule could call anything.
private func makeLeg(_ kernel: Kernel, id: String, canCall: [String]) async {
    var grants: JSON = .object([:])
    for t in canCall { grants[t] = .string("*") }
    _ = await kernel.send([Item("core", .object([
            "type": .string("create_agent"), "parent_id": .string("core"),
            "handler_module": .string("relay_connector.tools"), "id": .string(id),
            "can_call": grants,
        ]))])[0].result
    // The INGRESS half. `core` is the leg's parent, so `$children` already
    // covers it; a SIBLING needs the edge opened by name.
    for t in canCall where t != "core" {
        var accept: JSON = .object([
            "$self": .string("*"), "$parent": .string("*"), "$children": .string("*"),
        ])
        accept[id] = .string("*")
        _ = await kernel.send([Item("core", .object([
                "type": .string("update_agent"), "id": .string(t),
                "callable_by": accept,
            ]))])[0].result
    }
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
            let kernelA = await makeKernel(withEcho: false, leg: "brA")
            let kernelB = await makeKernel(withEcho: true, leg: "brB")
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
            let kernelA = await makeKernel(withEcho: false, leg: "brA")
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
            _ = await kernelB.send([Item("core", [
                    "type": "create_agent", "parent_id": "core", "handler_module": "file_bridge.tools",
                    "id": "fs", "root": .string(rel), "ingress_rule": "allow_all_ingress",
                ])])[0].result
            registry.register("relay_connector.tools", KernelBridgeBundle())
            await makeLeg(kernelB, id: "brB", canCall: ["fs"])

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
            let kernelA = await makeKernel(withEcho: false, leg: "brA")
            let kernelB = await makeKernel(withEcho: true, leg: "brB")
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
        // ONE bare stem, as `auth` carries it; each manifold appends its own suffix.
        let stem: JSON = ["type": "password", "env": .string(env)]
        try await withDeadline(15) {
            let kernelA = await makeKernel(withEcho: false, leg: "brA")
            let kernelB = await makeKernel(withEcho: true, leg: "brB")
            // A presents the group token (egress); B checks it (ingress). Both pass.
            let (transportA, transportB) = await pairTransports(
                kernelA: kernelA, kernelB: kernelB,
                egressA: try EgressRules.resolve(manifoldSpec(nil, stem, "_egress")),
                ingressB: try IngressRules.resolve(manifoldSpec(nil, stem, "_ingress")))
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
            let kernelA = await makeKernel(withEcho: false, leg: "brA")
            let kernelB = await makeKernel(withEcho: true, leg: "brB")
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

@Suite("io rule names are manifold-scoped")
struct ManifoldScopedRuleNameTests {
    /// Rule names bind to exactly ONE direction, so a name can never half-resolve
    /// across the two registries.
    ///
    /// Measured 2026-08-21, before scoping (python AND swift, which carried the
    /// identical aliases): `auth:"silent"` — a legitimate EGRESS policy — resolved
    /// ingress to `DenyInbound(unknown: "silent")`, SEALING the leg while recording the
    /// unknown name and saying nothing; and `auth:"allow_all"` handed egress a `Silent`
    /// nobody asked for. One unscoped name against two registries with different
    /// membership is a fallback, and this kernel has none.
    ///
    /// It still DENIES — an unresolvable rule is not an authorization — but it now names
    /// the manifold mistake instead of looking like an ordinary seal.
    @Test func anEgressNameInIngressRuleIsLoudNotSilent() throws {
        let sealed = try IngressRules.resolve(.string("silent_egress"))
        let d = sealed.authorize(
            AuthAction(kind: "call", target: "t", verb: "read", token: nil))
        guard case .deny(let denial) = d else {
            Issue.record("an egress name in ingress_rule must deny")
            return
        }
        // The bad NAME is the reason; the legal names + how to open the edge are
        // the hint the rule now carries with it.
        #expect(denial.reason.contains("silent_egress"))
        #expect(denial.hint.contains("manifold-scoped"))
        #expect(denial.hint.contains("allow_all_ingress"))
    }

    /// The `auth` STEM is scoped per manifold, so an egress-only stem cannot silently
    /// seal ingress: it surfaces as THIS manifold's missing rule, by name.
    @Test func theAuthStemIsScopedPerManifold() throws {
        #expect(manifoldSpec(nil, .string("silent"), "_ingress") == .string("silent_ingress"))
        // An inbound-only stem presents NOTHING outbound — a meaning spelled out per
        // stem, not a lookup miss.
        for stem in ["allow_all", "deny"] {
            #expect(manifoldSpec(nil, .string(stem), "_egress") == .string("silent_egress"))
        }
        // And the stems that DO span both manifolds still work from one field.
        for stem in ["password", "allow_all"] {
            _ = try IngressRules.resolve(manifoldSpec(nil, .string(stem), "_ingress"))
        }
    }
}
