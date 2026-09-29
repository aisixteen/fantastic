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
import OrderedCollections
import Testing

// ── kernels ────────────────────────────────────────────────────

private final class EchoBundle: AgentBundle {
    let name = "echo"
    func handle(agentId: AgentId, payload: JSON, kernel: Kernel) async throws -> JSON? {
        .object(["echoed": payload])
    }
}

private func makeKernel(
    withEcho: Bool, leg: String? = nil, ingress: JSON? = nil, egress: JSON? = nil,
    auth: JSON? = nil, guid: String? = nil, peerGuid: String? = nil
) async -> (Kernel, KernelBridgeBundle) {
    let registry = BundleRegistry()
    if withEcho { registry.register("echo.tools", EchoBundle()) }
    let bundle = KernelBridgeBundle(family: .relay)
    registry.register("relay_connector.tools", bundle)
    let kernel = rootedKernel(registry, guid: guid)
    if withEcho {
        _ = await kernel.send([
            Item(
                "kernel_state",
                [
                    "type": "create_agent", "parent_id": "kernel_state",
                    "handler_module": "echo.tools",
                    "id": "echo",
                ])
        ])[0].result
    }
    if let leg {
        await makeLeg(
            kernel, id: leg, canCall: withEcho ? ["echo"] : ["kernel_state"], ingress: ingress,
            egress: egress, auth: auth, peerGuid: peerGuid)
    }
    return (kernel, bundle)
}

/// A kernel whose root carries `guid` — the half of `<guid>|<agent>` its legs
/// stamp as `caller` on every outbound call.
private func rootedKernel(_ registry: BundleRegistry, guid: String?) -> Kernel {
    let kernel = Kernel(storage: .inMemory, bundles: registry)
    var meta: OrderedDictionary<String, JSON> = [:]
    if let guid { meta["guid"] = .string(guid) }
    let root = Agent(
        id: "kernel_state", handlerModule: nil, parentId: nil, meta: meta,
        rootPath: URL(fileURLWithPath: FileManager.default.currentDirectoryPath)
            .appendingPathComponent(".fantastic"))
    kernel.register(root)
    kernel.setRoot(root)
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
private func makeLeg(
    _ kernel: Kernel, id: String, canCall: [String], ingress: JSON? = nil, egress: JSON? = nil,
    auth: JSON? = nil, peerGuid: String? = nil
) async {
    var grants: JSON = .object([:])
    for t in canCall { grants[t] = .string("*") }
    var create: JSON = .object([
        "type": .string("create_agent"), "parent_id": .string("kernel_state"),
        "handler_module": .string("relay_connector.tools"), "id": .string(id),
        "can_call": grants,
        // python's test seam: `memory` is the kind `boot` POPS an injected
        // transport for. The per-leg rules come off the RECORD, as in production.
        "transport": .string("memory"),
    ])
    if let ingress { create["ingress_rule"] = ingress }
    if let egress { create["egress_rule"] = egress }
    if let auth { create["auth"] = auth }
    // The ONE peer this leg serves inbound calls for (audit 2026-09-28).
    if let peerGuid { create["peer_guid"] = .string(peerGuid) }
    _ = await kernel.send([Item("kernel_state", create)])[0].result
    // The INGRESS half. An inbound call is judged as its FOREIGN caller
    // (`<peer guid>|<agent>`), so every target opens the edge to `<peer guid>|*`;
    // a local sibling call through the leg still needs the leg by name.
    for t in canCall where t != "kernel_state" || peerGuid != nil {
        var accept: JSON = .object([
            "$self": .string("*"), "$parent": .string("*"), "$children": .string("*"),
        ])
        accept[id] = .string("*")
        if let peerGuid { accept["\(peerGuid)|*"] = .string("*") }
        _ = await kernel.send([
            Item(
                "kernel_state",
                .object([
                    "type": .string("update_agent"), "id": .string(t),
                    "callable_by": accept,
                ]))
        ])[0].result
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
            guard let (env, body) = try? Codec.decodeBinaryFrame(Data(bytes)),
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

/// Wire two relay legs (A↔B) through a fresh hub and BOOT them through the
/// engine — the transport is injected the way python's `_test_transport_inject`
/// seam does it, so the verbs, correlation and the inbound gate under test are
/// the engine's, not a transport shortcut.
private func pairLegs(
    kernelA: Kernel, bundleA: KernelBridgeBundle,
    kernelB: Kernel, bundleB: KernelBridgeBundle
) async {
    let hub = MemoryRelayHub()
    let wireA = await hub.register("A")
    let wireB = await hub.register("B")
    bundleA.injectTransport(
        agentId: "brA", transport: await RelayTransport.attach(wire: wireA, partnerGuid: "B"))
    bundleB.injectTransport(
        agentId: "brB", transport: await RelayTransport.attach(wire: wireB, partnerGuid: "A"))
    _ = await kernelA.send([Item("brA", ["type": "boot"])])[0].result
    _ = await kernelB.send([Item("brB", ["type": "boot"])])[0].result
}

/// `forward` through the leg agent, as any caller reaches it.
private func forward(_ kernel: Kernel, _ leg: String, target: String, payload: JSON) async -> JSON {
    await kernel.send([
        Item(
            AgentId(leg),
            .object([
                "type": .string("forward"), "target": .string(target), "payload": payload,
            ]))
    ])[0].result
}

private func binaryForward(
    _ kernel: Kernel, _ leg: String, target: String, payload: JSON, blob: Data
) async -> (JSON, Data?) {
    await kernel.sendWithBinary(
        AgentId(leg),
        .object([
            "type": .string("forward"), "target": .string(target), "payload": payload,
        ]), blob)
}

/// Tear both legs down (python `on_delete` → the engine's `_teardown`).
private func dropLegs(_ kernelA: Kernel, _ kernelB: Kernel) async {
    _ = await kernelA.send([Item("kernel_state", ["type": "delete_agent", "id": "brA"])])[0].result
    _ = await kernelB.send([Item("kernel_state", ["type": "delete_agent", "id": "brB"])])[0].result
}

// ── transport tests ────────────────────────────────────────────

@Suite("relay_connector transport")
struct RelayTransportTests {
    @Test func forwardRoundTripsOverRelay() async throws {
        try await withDeadline(15) {
            let open: JSON = .string("allow_all_ingress")
            let (kernelA, bundleA) = await makeKernel(
                withEcho: false, leg: "brA", ingress: open, guid: "gA", peerGuid: "gB")
            let (kernelB, bundleB) = await makeKernel(
                withEcho: true, leg: "brB", ingress: open, guid: "gB", peerGuid: "gA")
            await pairLegs(
                kernelA: kernelA, bundleA: bundleA, kernelB: kernelB, bundleB: bundleB)

            // 1) forward A → B's "echo" agent → reply tunnels back.
            let r1 = await forward(
                kernelA, "brA", target: "echo", payload: ["type": "echo", "msg": "hi"])
            #expect(r1["echoed"]["msg"].asString == "hi", "\(r1.serialize())")

            // 2) >64 KiB frame survives.
            let big = String(repeating: "x", count: 200_000)
            let r2 = await forward(
                kernelA, "brA", target: "echo", payload: ["type": "echo", "blob": .string(big)])
            #expect(r2["echoed"]["blob"].asString?.count == big.count)

            // 3) reverse: B → A's "kernel_state" (both legs serve inbound calls — symmetric).
            let r3 = await forward(
                kernelB, "brB", target: "kernel_state", payload: ["type": "list_agents"])
            let names = (r3["agents"].asArray ?? []).compactMap { $0["id"].asString }
            #expect(names.contains("kernel_state"), "\(r3.serialize())")

            await dropLegs(kernelA, kernelB)
        }
    }

    @Test func binaryForwardStreamsRawBytesOverRelay() async throws {
        try await withDeadline(20) {
            let open: JSON = .string("allow_all_ingress")
            let (kernelA, bundleA) = await makeKernel(
                withEcho: false, leg: "brA", ingress: open, guid: "gA", peerGuid: "gB")
            // kernelB hosts an OPEN file_bridge in a cwd-relative dir.
            let registry = BundleRegistry()
            registry.register("file_bridge.tools", FileBundle())
            let bundleB = KernelBridgeBundle(family: .relay)
            registry.register("relay_connector.tools", bundleB)
            let kernelB = rootedKernel(registry, guid: "gB")
            let fm = FileManager.default
            let rel = "fantastic-relaystream-\(UUID().uuidString)"
            let dir = URL(fileURLWithPath: fm.currentDirectoryPath).appendingPathComponent(rel)
            try fm.createDirectory(at: dir, withIntermediateDirectories: true)
            defer { try? fm.removeItem(at: dir) }
            _ = await kernelB.send([
                Item(
                    "kernel_state",
                    [
                        "type": "create_agent", "parent_id": "kernel_state",
                        "handler_module": "file_bridge.tools", "allow": ["."],
                        "id": "fs", "root": .string(rel), "ingress_rule": "allow_all_ingress",
                    ])
            ])[0].result
            await makeLeg(kernelB, id: "brB", canCall: ["fs"], ingress: open, peerGuid: "gA")

            await pairLegs(
                kernelA: kernelA, bundleA: bundleA, kernelB: kernelB, bundleB: bundleB)

            let payload = Data([0x00, 0xFF, 0xCA, 0xFE, 0xBA, 0xBE, 0x10, 0x80])
            // write_stream (binary) over the relay tunnel → kernelB.fs
            let (w, _) = await binaryForward(
                kernelA, "brA", target: "fs",
                payload: ["type": "write_stream", "path": "blob.bin", "truncate": .bool(true)],
                blob: payload)
            #expect(w["written"].asInt == Int64(payload.count), "\(w.serialize())")
            // read_stream (binary) over the relay tunnel → raw bytes back
            let (_, body) = await binaryForward(
                kernelA, "brA", target: "fs",
                payload: ["type": "read_stream", "path": "blob.bin"], blob: Data())
            #expect(body == payload, "raw bytes must round-trip over the relay tunnel")

            await dropLegs(kernelA, kernelB)
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

    /// A7.20/A7.21 — an UNBOOTED relay leg seals by default and reflects python's
    /// key order, family fields included.
    @Test func bridgeReflectKeyOrderMatchesPython() async throws {
        let (kernel, _) = await makeKernel(withEcho: false)
        _ = await kernel.send([
            Item(
                "kernel_state",
                .object([
                    "type": .string("create_agent"), "parent_id": .string("kernel_state"),
                    "handler_module": .string("relay_connector.tools"), "id": .string("rc"),
                    "transport": .string("relay"),
                    "relay_url": .string("ws://localhost:9/"), "partner_guid": .string("peer"),
                ]))
        ])[0].result
        let r = await kernel.send([Item("rc", ["type": "reflect"])])[0].result
        // The kernel appends its own reflect flags (can_call, kernel_verbs, tree)
        // after the bundle's keys, so the BUNDLE half is the prefix.
        #expect(
            Array((r.asObject?.keys ?? []).prefix(18)) == [
                "id", "sentence", "transport", "connected", "pending_count", "ingress_rule",
                "egress_rule", "sealed", "peer_guid", "relay_url", "guid", "partner_guid",
                "reconnect", "role", "owner_guid", "exposes", "verbs", "emits",
            ], "\(r.serialize())")
        #expect(r["transport"].asString == "relay")
        #expect(r["connected"].asBool == false && r["pending_count"].asInt == 0)
        #expect(r["ingress_rule"].asString == "deny_ingress" && r["sealed"].asBool == true)
        #expect(r["egress_rule"].asString == "silent_egress")
        #expect(r["reconnect"] == .double(10.0) && r["role"].asString == "kernel")
        #expect(r["exposes"] == .array([]) && r["owner_guid"].isNull)
        #expect(r["verbs"].asObject?.count == 10)
        #expect(r["emits"].asObject?.keys.contains("bridge_up") == true)
    }

    /// A7.20/A7.21 — the ws derivation's own field set and six verbs.
    @Test func wsBridgeReflectFieldsMatchPython() async throws {
        let registry = BundleRegistry()
        registry.register("ws_bridge.tools", KernelBridgeBundle(family: .ws))
        let kernel = Kernel(storage: .inMemory, bundles: registry)
        let root = Agent(id: "kernel_state", handlerModule: nil, parentId: nil)
        kernel.register(root)
        kernel.setRoot(root)
        _ = await kernel.send([
            Item(
                "kernel_state",
                .object([
                    "type": .string("create_agent"), "parent_id": .string("kernel_state"),
                    "handler_module": .string("ws_bridge.tools"), "id": .string("wb"),
                    "peer_id": .string("kernel_state"), "port": .integer(8123),
                ]))
        ])[0].result
        let r = await kernel.send([Item("wb", ["type": "reflect"])])[0].result
        #expect(
            Array((r.asObject?.keys ?? []).prefix(14)) == [
                "id", "sentence", "transport", "connected", "pending_count", "ingress_rule",
                "egress_rule", "sealed", "peer_guid", "host", "peer_id", "port", "verbs",
                "emits",
            ], "\(r.serialize())")
        #expect(r["transport"].asString == "ws")
        // A7.24 — the dial to a dead port FAILED, so nothing attached: boot is
        // not a half-success that reflects `connected:true` and times out later.
        #expect(r["connected"].asBool == false)
        #expect(r["host"].isNull && r["peer_id"].asString == "kernel_state")
        #expect(r["port"].asInt == 8123)
        #expect(r["peer_guid"].isNull)
        #expect(r["verbs"].asObject?.count == 6)
        #expect(
            r["sentence"].asString
                == "Cross-kernel comms bridge — WS-only, asymmetric (no peer bridge needed); weak proxy."
        )
    }

    /// A7.24 — a relay leg validates `relay_url` + `partner_guid` BEFORE minting
    /// the guid, so a half-configured leg leaves no durable identity behind.
    @Test func relayBootMissingPartnerMintsNoGuid() async throws {
        let (kernel, _) = await makeKernel(withEcho: false)
        _ = await kernel.send([
            Item(
                "kernel_state",
                .object([
                    "type": .string("create_agent"), "parent_id": .string("kernel_state"),
                    "handler_module": .string("relay_connector.tools"), "id": .string("rc2"),
                    "transport": .string("relay"),
                    "relay_url": .string("ws://localhost:9/"),
                ]))
        ])[0].result
        let booted = await kernel.send([Item("rc2", ["type": "boot"])])[0].result
        #expect(
            booted
                == .object([
                    "error": .string(
                        "bridge: relay boot failed: relay_connector requires relay_url, partner_guid"
                    )
                ]), "\(booted.serialize())")
        let r = await kernel.send([Item("rc2", ["type": "reflect"])])[0].result
        #expect(r["guid"].isNull, "a failed boot must mint no guid: \(r.serialize())")
    }

    /// A7.1 — the two readmes are python's, byte for byte.
    @Test func bridgeReadmesMatchPythonVerbatim() throws {
        let ws = try String(
            contentsOf: pythonSource("io/ws_bridge/src/ws_bridge/readme.md"), encoding: .utf8)
        let relay = try String(
            contentsOf: pythonSource("io/relay_connector/src/relay_connector/readme.md"),
            encoding: .utf8)
        #expect(KernelBridgeBundle(family: .ws).readme == ws)
        #expect(KernelBridgeBundle(family: .relay).readme == relay)
    }

    /// A7.2/A7.5 — an unknown type (and a directory verb on a ws leg, and
    /// `shutdown`) answers python's envelope-teaching miss.
    @Test func unknownTypeTeachesTheEnvelope() async throws {
        let registry = BundleRegistry()
        registry.register("ws_bridge.tools", KernelBridgeBundle(family: .ws))
        let kernel = Kernel(storage: .inMemory, bundles: registry)
        let root = Agent(id: "kernel_state", handlerModule: nil, parentId: nil)
        kernel.register(root)
        kernel.setRoot(root)
        _ = await kernel.send([
            Item(
                "kernel_state",
                .object([
                    "type": .string("create_agent"), "parent_id": .string("kernel_state"),
                    "handler_module": .string("ws_bridge.tools"), "id": .string("wb"),
                ]))
        ])[0].result
        for verb in ["zap", "shutdown", "list_peers", "set_identity"] {
            let r = await kernel.send([Item("wb", .object(["type": .string(verb)]))])[0].result
            #expect(r["error"].asString == "bridge: unknown type '\(verb)'", "\(r.serialize())")
            #expect(r["hint"].asString?.hasPrefix("every bridge frame needs a") == true)
        }
    }

    /// A7.23/A7.24/A7.25 — boot texts, and every pre-boot verb's refusal.
    @Test func bridgeBootAndForwardTextsMatchPython() async throws {
        let registry = BundleRegistry()
        registry.register("ws_bridge.tools", KernelBridgeBundle(family: .ws))
        let kernel = Kernel(storage: .inMemory, bundles: registry)
        let root = Agent(id: "kernel_state", handlerModule: nil, parentId: nil)
        kernel.register(root)
        kernel.setRoot(root)
        _ = await kernel.send([
            Item(
                "kernel_state",
                .object([
                    "type": .string("create_agent"), "parent_id": .string("kernel_state"),
                    "handler_module": .string("ws_bridge.tools"), "id": .string("wb"),
                ]))
        ])[0].result
        // A leg with no `peer_id`: `bridge: <kind> boot failed: <ValueError text>`.
        let booted = await kernel.send([Item("wb", ["type": "boot"])])[0].result
        #expect(
            booted
                == .object([
                    "error": .string(
                        "bridge: ws boot failed: peer_id required for the ws transport")
                ]), "\(booted.serialize())")
        // memory needs the test seam.
        _ = await kernel.send([
            Item(
                "kernel_state",
                .object([
                    "type": .string("update_agent"), "id": .string("wb"),
                    "transport": .string("memory"),
                ]))
        ])[0].result
        let mem = await kernel.send([Item("wb", ["type": "boot"])])[0].result
        #expect(
            mem == .object(["error": .string("bridge: memory transport requires test injection")]),
            "\(mem.serialize())")
        // forward validation, then the not-connected refusal.
        let badArgs = await kernel.send([Item("wb", ["type": "forward", "target": "x"])])[0].result
        #expect(
            badArgs
                == .object([
                    "error": .string("bridge.forward: target (str) + payload (dict) required")
                ]), "\(badArgs.serialize())")
        let cold = await kernel.send([
            Item(
                "wb",
                .object([
                    "type": .string("forward"), "target": .string("x"),
                    "payload": .object(["type": .string("reflect")]),
                ]))
        ])[0].result
        #expect(cold["error"].asString == "bridge.forward: not connected")
        #expect(
            cold["reason"].asString == "not_connected" && cold["hint"].asString == "call boot first"
        )
        let watch = await kernel.send([Item("wb", ["type": "watch_remote", "target": "x"])])[0]
            .result
        #expect(watch["error"].asString == "bridge.watch_remote: not connected")
        let noTarget = await kernel.send([Item("wb", ["type": "unwatch_remote"])])[0].result
        #expect(
            noTarget == .object(["error": .string("bridge.unwatch_remote: target (str) required")]),
            "\(noTarget.serialize())")
    }

    /// A7.29 — `unwatch_directory` before boot is simply OK; a bad `set_identity`
    /// role is python's `bad_arg`.
    @Test func directoryRepliesMatchPython() async throws {
        let (kernel, bundle) = await makeKernel(withEcho: false, leg: "brA")
        let pre = await kernel.send([Item("brA", ["type": "unwatch_directory"])])[0].result
        #expect(pre == .object(["ok": .bool(true)]), "\(pre.serialize())")
        let coldIdentity = await kernel.send([Item("brA", ["type": "set_identity"])])[0].result
        #expect(coldIdentity["error"].asString == "relay_connector.set_identity: not connected")
        let hub = MemoryRelayHub()
        let wire = await hub.register("A")
        bundle.injectTransport(
            agentId: "brA", transport: await RelayTransport.attach(wire: wire, partnerGuid: "B"))
        _ = await kernel.send([Item("brA", ["type": "boot"])])[0].result
        let badRole = await kernel.send([Item("brA", ["type": "set_identity", "role": "boss"])])[0]
            .result
        #expect(
            badRole["error"].asString
                == "set_identity: role must be one of ('manager', 'kernel')",
            "\(badRole.serialize())")
        #expect(badRole["reason"].asString == "bad_arg" && badRole["arg"].asString == "role")
        _ = await kernel.send([Item("kernel_state", ["type": "delete_agent", "id": "brA"])])[0]
            .result
    }

    /// A7.27 — the correlator is namespaced by agent id (`<bridge_id>-<n>`).
    @Test func corrIdsAreNamespacedByAgentId() async throws {
        let (kernel, bundle) = await makeKernel(withEcho: false, leg: "brA")
        let hub = MemoryRelayHub()
        let wire = await hub.register("A")
        let spy = await hub.register("B")
        bundle.injectTransport(
            agentId: "brA", transport: await RelayTransport.attach(wire: wire, partnerGuid: "B"))
        _ = await kernel.send([Item("brA", ["type": "boot"])])[0].result
        // No partner answers, so the forward times out — the FRAME is what this pins.
        async let _ = forward(
            kernel, "brA", target: "x",
            payload: ["type": "reflect"])
        let msg = try await spy.receive()
        guard case .text(let s) = msg, let env = try? JSON.parse(s) else {
            Issue.record("expected a text relay envelope")
            return
        }
        #expect(env["payload"]["id"].asString == "brA-1", "\(s)")
        _ = await kernel.send([Item("kernel_state", ["type": "delete_agent", "id": "brA"])])[0]
            .result
    }

    /// A7.30/A7.17 — `bridge_up` after boot, exactly one `bridge_down` when the
    /// leg goes (the read loop's tail).
    @Test func bridgeEmitsBridgeUpAndDown() async throws {
        let (kernel, bundle) = await makeKernel(withEcho: false, leg: "brA")
        let inbox = kernel.ensureInbox("leg_watcher")
        kernel.watch(src: "brA", watcher: "leg_watcher")
        let hub = MemoryRelayHub()
        let wire = await hub.register("A")
        bundle.injectTransport(
            agentId: "brA", transport: await RelayTransport.attach(wire: wire, partnerGuid: "B"))
        _ = await kernel.send([Item("brA", ["type": "boot"])])[0].result
        _ = await kernel.send([Item("kernel_state", ["type": "delete_agent", "id": "brA"])])[0]
            .result
        var seen: [String] = []
        var iter = inbox.makeAsyncIterator()
        let deadline = Date().addingTimeInterval(3)
        while Date() < deadline, seen.count < 2, let ev = await iter.next() {
            // The leg's inbox also carries the kernel's own lifecycle events; the
            // bridge's own pair is what this pins.
            if let t = ev.payload["type"].asString, t.hasPrefix("bridge_") { seen.append(t) }
        }
        #expect(seen == ["bridge_up", "bridge_down"], "\(seen)")
    }

    /// A7.31 — the read loop is REGISTERED work: `ps` carries a `read_loop` row
    /// for the booted leg.
    @Test func bootedBridgePsRowsMatchPython() async throws {
        let (kernel, bundle) = await makeKernel(withEcho: false, leg: "brA")
        let hub = MemoryRelayHub()
        let wire = await hub.register("A")
        bundle.injectTransport(
            agentId: "brA", transport: await RelayTransport.attach(wire: wire, partnerGuid: "B"))
        _ = await kernel.send([Item("brA", ["type": "boot"])])[0].result
        let ps = await kernel.send([Item("kernel", ["type": "ps"])])[0].result
        #expect(
            (ps["tasks"].asArray ?? []).contains {
                $0["agent"].asString == "brA" && $0["verb"].asString == "read_loop"
            }, "\(ps.serialize())")
        _ = await kernel.send([Item("kernel_state", ["type": "delete_agent", "id": "brA"])])[0]
            .result
    }

    /// A7.34 — an `error` frame with no `error` field is python's `str(None)`.
    /// A7.3 — a SUBSCRIBED `event` frame with no payload is dropped (the payload
    /// must be an object); the next one re-emits as its payload plus `src`.
    @Test(.timeLimit(.minutes(1)))
    func errorFrameWithoutErrorSaysNoneAndAnEmptyEventReEmits() async throws {
        let (kernel, bundle) = await makeKernel(withEcho: false, leg: "brA")
        let inbox = kernel.ensureInbox("leg_watcher2")
        kernel.watch(src: "brA", watcher: "leg_watcher2")
        let hub = MemoryRelayHub()
        let wire = await hub.register("A")
        let peer = await hub.register("B")
        bundle.injectTransport(
            agentId: "brA", transport: await RelayTransport.attach(wire: wire, partnerGuid: "B"))
        _ = await kernel.send([Item("brA", ["type": "boot"])])[0].result
        // Nothing re-emits until subscribed: the leg watches `sensor` first, and
        // the partner reads that `watch` frame before the forward.
        _ = await kernel.send([Item("brA", ["type": "watch_remote", "target": "sensor"])])[0]
            .result
        _ = try await peer.receive()
        // The partner answers the forward with a bare `error` frame.
        Task {
            let msg = try? await peer.receive()
            guard case .text(let raw)? = msg, let env = try? JSON.parse(raw) else { return }
            let id = env["payload"]["id"]
            try? await peer.send(
                .text(
                    JSON.object([
                        "type": .string("send"), "target": .string("A"),
                        "payload": .object(["type": .string("error"), "id": id]),
                    ]).serialize()))
            // …then a payload-less event from the subscribed src (dropped), then
            // one with a payload (re-emitted) — the transport keeps their order.
            try? await peer.send(
                .text(
                    JSON.object([
                        "type": .string("send"), "target": .string("A"),
                        "payload": .object(["type": .string("event"), "src": .string("sensor")]),
                    ]).serialize()))
            try? await peer.send(
                .text(
                    JSON.object([
                        "type": .string("send"), "target": .string("A"),
                        "payload": .object([
                            "type": .string("event"), "src": .string("sensor"),
                            "payload": .object(["n": .integer(2)]),
                        ]),
                    ]).serialize()))
        }
        let r = await forward(kernel, "brA", target: "x", payload: ["type": "reflect"])
        #expect(r == .object(["error": .string("remote error: None")]), "\(r.serialize())")
        var first: JSON?
        var iter = inbox.makeAsyncIterator()
        while let ev = await iter.next() {
            if ev.payload["src"].asString == "sensor" {
                first = ev.payload
                break
            }
        }
        #expect(
            first == .object(["n": .integer(2), "src": .string("sensor")]),
            "the payload-less event must be dropped: \(first?.serialize() ?? "nil")")
        _ = await kernel.send([Item("kernel_state", ["type": "delete_agent", "id": "brA"])])[0]
            .result
    }

    /// A7.25/A7.26 — the `timeout` arg bounds the forward, rendered as python's
    /// float repr.
    @Test func bridgeForwardHonoursTimeoutArg() async throws {
        let (kernel, bundle) = await makeKernel(withEcho: false, leg: "brA")
        let hub = MemoryRelayHub()
        let wire = await hub.register("A")
        _ = await hub.register("B")  // a partner that never answers
        bundle.injectTransport(
            agentId: "brA", transport: await RelayTransport.attach(wire: wire, partnerGuid: "B"))
        _ = await kernel.send([Item("brA", ["type": "boot"])])[0].result
        let started = Date()
        let r = await kernel.send([
            Item(
                "brA",
                .object([
                    "type": .string("forward"), "target": .string("x"),
                    "payload": .object(["type": .string("reflect")]),
                    "timeout": .double(0.2),
                ]))
        ])[0].result
        #expect(
            r == .object(["error": .string("bridge.forward: timeout after 0.2s")]),
            "\(r.serialize())")
        #expect(Date().timeIntervalSince(started) < 5)
        _ = await kernel.send([Item("kernel_state", ["type": "delete_agent", "id": "brA"])])[0]
            .result
    }

    /// A7.33 — teardown closes the transport and fails every pending forward
    /// with python's `ConnectionError` text.
    @Test func bridgeDetachClosesAndFailsPending() async throws {
        let (kernel, bundle) = await makeKernel(withEcho: false, leg: "brA")
        let hub = MemoryRelayHub()
        let wire = await hub.register("A")
        _ = await hub.register("B")
        bundle.injectTransport(
            agentId: "brA", transport: await RelayTransport.attach(wire: wire, partnerGuid: "B"))
        _ = await kernel.send([Item("brA", ["type": "boot"])])[0].result
        async let pending = forward(kernel, "brA", target: "x", payload: ["type": "reflect"])
        try await Task.sleep(nanoseconds: 100_000_000)
        _ = await kernel.send([Item("kernel_state", ["type": "delete_agent", "id": "brA"])])[0]
            .result
        let r = await pending
        #expect(
            r == .object(["error": .string("bridge.forward: bridge transport closed")]),
            "\(r.serialize())")
    }

    /// A7.4/A7.32 — an inbound call for an unknown target gets the KERNEL's own
    /// refusal, not a bridge-invented text, and the read loop keeps reading
    /// (each call is its own task).
    @Test func inboundCallToAnUnknownTargetRepliesTheKernelRefusal() async throws {
        try await withDeadline(15) {
            let open: JSON = .string("allow_all_ingress")
            let (kernelA, bundleA) = await makeKernel(
                withEcho: false, leg: "brA", ingress: open, guid: "gA", peerGuid: "gB")
            let (kernelB, bundleB) = await makeKernel(
                withEcho: true, leg: "brB", ingress: open, guid: "gB", peerGuid: "gA")
            await pairLegs(
                kernelA: kernelA, bundleA: bundleA, kernelB: kernelB, bundleB: bundleB)
            let miss = await forward(
                kernelA, "brA", target: "nope", payload: ["type": "reflect"])
            #expect(miss["reason"].asString == "unknown_agent", "\(miss.serialize())")
            // The loop survived it — the next call still lands.
            let ok = await forward(
                kernelA, "brA", target: "echo", payload: ["type": "echo", "msg": "still here"])
            #expect(ok["echoed"]["msg"].asString == "still here", "\(ok.serialize())")
            await dropLegs(kernelA, kernelB)
        }
    }

    @Test func denyInboundRefusesReverseCall() async throws {
        try await withDeadline(15) {
            let open: JSON = .string("allow_all_ingress")
            let (kernelA, bundleA) = await makeKernel(
                withEcho: false, leg: "brA", ingress: open, guid: "gA", peerGuid: "gB")
            // B's leg is SEALED (no `ingress_rule` — python's default).
            let (kernelB, bundleB) = await makeKernel(
                withEcho: true, leg: "brB", guid: "gB", peerGuid: "gA")
            await pairLegs(
                kernelA: kernelA, bundleA: bundleA, kernelB: kernelB, bundleB: bundleB)
            let r = await forward(kernelA, "brA", target: "echo", payload: ["type": "echo"])
            #expect(r["reason"].asString == "unauthorized", "\(r.serialize())")
            await dropLegs(kernelA, kernelB)
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
            // A presents the group token (egress); B checks it (ingress). Both pass.
            let (kernelA, bundleA) = await makeKernel(
                withEcho: false, leg: "brA", auth: stem, guid: "gA", peerGuid: "gB")
            let (kernelB, bundleB) = await makeKernel(
                withEcho: true, leg: "brB", auth: stem, guid: "gB", peerGuid: "gA")
            await pairLegs(
                kernelA: kernelA, bundleA: bundleA, kernelB: kernelB, bundleB: bundleB)
            let r = await forward(
                kernelA, "brA", target: "echo", payload: ["type": "echo", "msg": "hey"])
            #expect(r["echoed"]["msg"].asString == "hey", "\(r.serialize())")
            await dropLegs(kernelA, kernelB)
        }
    }

    @Test func passwordRejectsTokenlessCaller() async throws {
        let env = "FANTASTIC_GROUP_TOKEN_RELAY_UNIT2"
        setenv(env, "s3cret", 1)
        defer { unsetenv(env) }
        try await withDeadline(15) {
            // A presents NOTHING (silent egress); B requires the token → deny.
            let (kernelA, bundleA) = await makeKernel(
                withEcho: false, leg: "brA", ingress: .string("allow_all_ingress"), guid: "gA",
                peerGuid: "gB")
            let (kernelB, bundleB) = await makeKernel(
                withEcho: true, leg: "brB", auth: ["type": "password", "env": .string(env)],
                guid: "gB", peerGuid: "gA")
            await pairLegs(
                kernelA: kernelA, bundleA: bundleA, kernelB: kernelB, bundleB: bundleB)
            let r = await forward(kernelA, "brA", target: "echo", payload: ["type": "echo"])
            #expect(r["reason"].asString == "unauthorized", "\(r.serialize())")
            await dropLegs(kernelA, kernelB)
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

// ── A15.1 — a malformed inbound frame is skipped, not fatal ─────

/// A wire that hands back a scripted list of frames, python's `_FakeWS`.
private actor ScriptedWire: RelayWire {
    private var frames: [NIOWebSocketClient.Message]
    init(_ frames: [NIOWebSocketClient.Message]) { self.frames = frames }
    func send(_ message: NIOWebSocketClient.Message) async throws {}
    func receive() async throws -> NIOWebSocketClient.Message {
        if frames.isEmpty { throw NIOWebSocketClient.Closed(reason: "scripted wire drained") }
        return frames.removeFirst()
    }
    func close() async {}
}

private func binaryFrame(headerLen: UInt32, _ tail: [UInt8]) -> [UInt8] {
    var out: [UInt8] = [
        UInt8((headerLen >> 24) & 0xFF), UInt8((headerLen >> 16) & 0xFF),
        UInt8((headerLen >> 8) & 0xFF), UInt8(headerLen & 0xFF),
    ]
    out.append(contentsOf: tail)
    return out
}

// ── A7.22 — the self-healing re-dial starts at once, not after a backoff ──

/// Counts dial attempts and fails the first `failures` of them.
private actor DialScript {
    private var attempts = 0
    private let failures: Int
    init(failures: Int) { self.failures = failures }
    func count() -> Int { attempts }
    func dial() throws -> any RelayWire {
        attempts += 1
        if attempts <= failures {
            throw NIOWebSocketClient.Closed(reason: "relay down")
        }
        return ScriptedWire([
            .text(#"{"type": "event", "source": "B", "payload": {"ok": true}}"#)
        ])
    }
}

@Suite("relay self-healing")
struct RelaySelfHealTests {

    /// Python `_reconnect_ws` DIALS FIRST and sleeps only after a dial FAILS, so
    /// a leg that boots with no socket re-dials immediately. Swift slept one full
    /// `reconnect` interval before every attempt; with a 30 s backoff that is the
    /// difference between healing now and never healing inside the window.
    /// Measured on the parity `bridges` capture as `relay_connections` 1 vs
    /// python's 2.
    @Test func reDialHappensBeforeTheBackoffSleep() async throws {
        let script = DialScript(failures: 1)
        let transport = try await RelayTransport.connect(
            relayURL: URL(string: "ws://127.0.0.1:1")!, guid: "A", token: "",
            partnerGuid: "B", heartbeat: 0, reconnect: 30,
            dialer: { try await script.dial() })
        // The eager dial failed, so the leg booted with NO socket (python:
        // "boot still succeeds and the leg connects in the background"). The
        // 30 s backoff must not be charged before the first re-dial.
        let frame = try await withDeadline(5) { try await transport.recv().frame }
        #expect(frame["ok"].asBool == true)
        let attempts = await script.count()
        #expect(attempts == 2)
        await transport.close()
    }
}

@Suite("ws_bridge frame resilience")
struct WSFrameResilienceTests {

    /// Python `test_ws_transport_skips_malformed_frames_and_keeps_reading`: five
    /// undecodable frames, then a good one. The reader has to survive all five
    /// AND say why on stderr — raising killed the engine read loop while the
    /// transport stayed open, so the leg reflected `connected:true` and answered
    /// every forward with a 30s timeout that never named the dead reader.
    @Test func malformedFrameIsSkippedAndReaderSurvives() async throws {
        let good = #"{"type": "reply", "id": "1", "data": {"ok": true}}"#
        let transport = WSTransport(
            client: ScriptedWire([
                .binary([0x00, 0x00, 0x00]),  // shorter than the length prefix
                .binary(binaryFrame(headerLen: 9999, [0x7B, 0x7D])),  // header past the end
                .binary(binaryFrame(headerLen: 8, Array("not jsonbody".utf8))),  // bad header
                .text("{not json"),  // bad text frame
                .text("[1]"),  // well-formed JSON, not an envelope
                .text(good),
            ]))

        var frame: JSON = .null
        let lines = await RuntimeLog.capture {
            frame = try! await withDeadline(2) { try await transport.recv().frame }
        }
        #expect(frame["type"].asString == "reply")
        #expect(frame["id"].asString == "1")
        #expect(frame["data"]["ok"].asBool == true)

        // Python's two lines, one per skipped frame, in order.
        let skips = lines.filter { $0.hasPrefix("ws_bridge: skipping") }
        #expect(skips.count == 5)
        #expect(skips[0].hasPrefix("ws_bridge: skipping malformed frame: "))
        #expect(skips[0].contains("binary frame shorter than its 4-byte header (3B)"))
        #expect(
            skips[1].contains(
                "binary frame header claims 9999B but only 2B follow the length prefix"))
        #expect(skips[2].hasPrefix("ws_bridge: skipping malformed frame: bad binary frame header:"))
        #expect(skips[3].hasPrefix("ws_bridge: skipping malformed frame: bad text frame:"))
        #expect(skips[4] == "ws_bridge: skipping non-object frame\n")
    }
}
