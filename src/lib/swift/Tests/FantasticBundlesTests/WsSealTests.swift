// A ws leg to a NON-LOOPBACK host is SEALED for secrets (owner, 2026-10-03,
// audit S8) — the twin of python `ws_bridge/tests/test_ws_seal.py`.
//
// `ws://` carries every frame in clear, and a forward's secret values ride its
// frame verbatim (the far kernel needs them). So a leg dialed to a host other than
// this machine (`localhost` / `127.0.0.1` / `[::1]` — an ssh tunnel's local end)
// refuses any call carrying secret marks, in both directions, with the relay
// seal's shape: outbound, a marked `forward` is refused before a frame leaves;
// inbound, a `call` frame naming `secret` is answered with the same refusal and
// never dispatched.
//
// The socket is a scripted wire; a sealed transport (python's monkeypatched dial
// to a non-loopback host) is handed to the leg through the injection seam.

import FantasticIoBridge
import FantasticJSON
import FantasticKernel
import FantasticKernelBridge
import Foundation
import Testing

private let KEY = "sk-ws-seal-0123456789abcdef"

private let SEALED: JSON = [
    "error": .string(WS_SECRET_SEALED), "reason": "unauthorized",
    "hint": .string(WS_SECRET_SEALED_HINT),
]

/// A websocket peer: records every frame it receives (decoded), answers each
/// `call` with `{ok: true}`, and hands back what a test pushes.
private actor PeerWire: RelayWire {
    private(set) var seen: [JSON] = []
    private var inbox: [NIOWebSocketClient.Message] = []
    private var waiters: [CheckedContinuation<NIOWebSocketClient.Message, Error>] = []

    func send(_ message: NIOWebSocketClient.Message) async throws {
        guard case .text(let s) = message else { return }
        let frame = try JSON.parseStrict(s)
        seen.append(frame)
        if frame["type"].asString == "call" {
            push(["type": "reply", "id": frame["id"], "data": ["ok": true]])
        }
    }

    func push(_ frame: JSON) {
        let msg = NIOWebSocketClient.Message.text(frame.serialize())
        if waiters.isEmpty {
            inbox.append(msg)
        } else {
            waiters.removeFirst().resume(returning: msg)
        }
    }

    func receive() async throws -> NIOWebSocketClient.Message {
        if !inbox.isEmpty { return inbox.removeFirst() }
        return try await withCheckedThrowingContinuation { waiters.append($0) }
    }

    func close() async {
        let w = waiters
        waiters.removeAll()
        for c in w { c.resume(throwing: NIOWebSocketClient.Closed(reason: "websocket: closed")) }
    }
}

@Suite("ws_bridge — a leg to a non-loopback host is sealed for secrets")
struct WsSealTests {

    private func kernel() -> (Kernel, KernelBridgeBundle) {
        let registry = BundleRegistry()
        let bridge = KernelBridgeBundle(family: .ws)
        registry.register("ws_bridge.tools", bridge)
        let kernel = Kernel(storage: .inMemory, bundles: registry)
        let root = Agent(id: AgentId("kernel_state"), handlerModule: nil, parentId: nil)
        kernel.register(root)
        kernel.setRoot(root)
        return (kernel, bridge)
    }

    /// A leg whose live transport is `t` (the injection seam), booted.
    private func leg(_ kernel: Kernel, _ bridge: KernelBridgeBundle, _ t: WSTransport) async {
        let made = await kernel.send([
            Item(
                AgentId("kernel"),
                [
                    "type": "create_agent", "parent_id": "kernel_state",
                    "handler_module": "ws_bridge.tools", "id": "s_leg", "transport": "memory",
                    "peer_id": "web_ws",
                ])
        ])[0].result
        #expect(made["id"] == "s_leg", "\(made.serialize())")
        bridge.injectTransport(agentId: AgentId("s_leg"), transport: t)
        let booted = await kernel.send([Item(AgentId("s_leg"), ["type": "boot"])])[0].result
        #expect(booted == ["booted": true, "transport": "memory"], "\(booted.serialize())")
    }

    private func forward(_ kernel: Kernel, secret: JSON?) async -> Outcome {
        var it = Item(
            AgentId("s_leg"),
            [
                "type": "forward", "target": "vault",
                "payload": ["type": "set_key", "key": .string(KEY)], "timeout": 2,
            ])
        it.secret = secret
        return await kernel.send([it])[0]
    }

    @Test(arguments: [
        ("10.0.0.5", true), ("peer.example.test", true), ("0.0.0.0", true),
        ("localhost", false), ("127.0.0.1", false), ("[::1]", false),
    ])
    func theDialedHostDecidesTheSeal(host: String, sealed: Bool) {
        #expect(wsHostIsSealed(host) == sealed, "\(host)")
    }

    @Test(
        .timeLimit(.minutes(1)),
        arguments: [
            JSON.array(["payload.payload.key"]),
            JSON.array(["result"]),
            JSON.array(["payload.timeout"]),
            JSON.array([.array(["payload", "payload", "key"])]),
        ])
    func aMarkedForwardToANonLoopbackHostIsRefusedBeforeAFrameLeaves(secret: JSON) async {
        let (kernel, bridge) = kernel()
        let wire = PeerWire()
        await leg(kernel, bridge, WSTransport(client: wire, sealed: true))
        let out = await forward(kernel, secret: secret)
        #expect(out.status == .errored, "\(out.result.serialize())")
        #expect(out.result == SEALED, "\(out.result.serialize())")
        // the same leg still carries an UNMARKED forward — the seal is about marks
        let plain = await forward(kernel, secret: nil)
        #expect(plain.result == ["ok": true], "\(plain.result.serialize())")
        let seen = await wire.seen
        #expect(seen.count == 1, "only the unmarked call crossed: \(seen)")
        #expect(seen.first?["type"] == "call")
        #expect(seen.first?.asObject?["secret"] == nil)
        _ = await kernel.send([
            Item(AgentId("kernel_state"), ["type": "delete_agent", "id": "s_leg"])
        ])
    }

    @Test(.timeLimit(.minutes(1)))
    func aMarkedForwardOverALoopbackLegCarriesItsMarks() async {
        let (kernel, bridge) = kernel()
        let wire = PeerWire()
        await leg(kernel, bridge, WSTransport(client: wire, sealed: false))
        let out = await forward(kernel, secret: ["payload.payload.key"])
        #expect(out.result == ["ok": true], "\(out.result.serialize())")
        let seen = await wire.seen
        #expect(seen.count == 1, "\(seen)")
        #expect(seen.first?["secret"] == ["payload.key"])
        #expect(seen.first?["payload"]["key"] == .string(KEY))
        _ = await kernel.send([
            Item(AgentId("kernel_state"), ["type": "delete_agent", "id": "s_leg"])
        ])
    }

    /// Inbound: on a sealed leg a `call` frame naming `secret` is answered with
    /// the seal's refusal and never reaches the engine; the next frame is read on.
    /// A loopback leg hands the call to the engine.
    @Test(.timeLimit(.minutes(1)), arguments: [true, false])
    func anInboundMarkedCallOnANonLoopbackLegIsAnsweredSealed(sealed: Bool) async throws {
        let wire = PeerWire()
        let t = WSTransport(client: wire, sealed: sealed)
        let call: JSON = [
            "type": "call", "id": "sealed-1", "target": "vault",
            "payload": ["type": "set_key", "key": .string(KEY)],
            "caller": "peer-g|kernel", "secret": ["payload.key"],
        ]
        let after: JSON = ["type": "reply", "id": "x-1", "data": ["ok": true]]
        await wire.push(call)
        await wire.push(after)
        let got = try await t.recv().frame
        if sealed {
            #expect(got == after, "\(got.serialize())")
            #expect(await wire.seen == [["type": "reply", "id": "sealed-1", "data": SEALED]])
        } else {
            #expect(got == call, "\(got.serialize())")
            #expect(await wire.seen == [])
        }
    }

    @Test func theForwardDocNamesTheSeal() async {
        let (kernel, _) = kernel()
        let made = await kernel.send([
            Item(
                AgentId("kernel"),
                [
                    "type": "create_agent", "parent_id": "kernel_state",
                    "handler_module": "ws_bridge.tools", "id": "d_leg", "transport": "memory",
                ])
        ])[0].result
        #expect(made["id"] == "d_leg", "\(made.serialize())")
        let shown = await kernel.send([Item(AgentId("d_leg"), ["type": "reflect"])])[0].result
        let doc = shown["verbs"]["forward"].asString ?? ""
        #expect(
            doc.hasSuffix(
                " SEALED for secrets to a non-loopback host: a call carrying any secret mark "
                    + "is refused — ws:// carries every frame in clear."), "\(doc)")
    }
}
