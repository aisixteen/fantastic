// relay_connector's DIRECTORY surface and its pending-request table, pinned.
//
// The directory verbs (`list_peers`, `watch_directory`, `unwatch_directory`)
// speak to the relay's own `relay` agent with relay-level ids (`dir_<n>`), and
// each request parks until exactly ONE of {its reply, its timeout, the
// transport closing} settles it. That table is one of the three hand-written
// pending-reply correlators; these tests hold its texts (python
// `relay_connector/_relay.py`) and its exactly-once settle. Also: the relay
// reader skips frames it cannot use instead of dying.

import FantasticIoBridge
import FantasticJSON
import FantasticKernel
import FantasticKernelBridge
import Foundation
import OrderedCollections
import Testing

/// A relay socket whose directory answers come from a script. Every frame the
/// transport sends is recorded; a `call`/`watch` to `target:"relay"` is answered
/// with `{type:"reply", id, data}` when `answer` is set, and left hanging when
/// it is nil.
private actor DirectoryWire: RelayWire {
    private var inbox: [NIOWebSocketClient.Message] = []
    private var waiters: [CheckedContinuation<NIOWebSocketClient.Message, Error>] = []
    private var closed = false
    private(set) var sent: [String] = []
    let answer: JSON?
    let failSends: Bool

    init(answer: JSON? = nil, failSends: Bool = false, script: [NIOWebSocketClient.Message] = []) {
        self.answer = answer
        self.failSends = failSends
        self.inbox = script
    }

    struct SendFailed: Error, CustomStringConvertible {
        var description: String { "wire is down" }
    }

    func send(_ message: NIOWebSocketClient.Message) async throws {
        guard case .text(let s) = message else { return }
        if failSends { throw SendFailed() }
        sent.append(s)
        guard let env = try? JSON.parse(s), env["target"].asString == "relay",
            let rid = env["id"].asString, let answer
        else { return }
        deliver(
            .text(
                JSON.object(["type": .string("reply"), "id": .string(rid), "data": answer])
                    .serialize()))
    }

    func receive() async throws -> NIOWebSocketClient.Message {
        if !inbox.isEmpty { return inbox.removeFirst() }
        if closed { throw NIOWebSocketClient.Closed(reason: "directory wire closed") }
        return try await withCheckedThrowingContinuation { waiters.append($0) }
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
        for c in w {
            c.resume(throwing: NIOWebSocketClient.Closed(reason: "directory wire closed"))
        }
    }
}

/// A kernel with ONE relay leg `brA` (not booted).
private func legKernel() async -> (Kernel, KernelBridgeBundle) {
    let registry = BundleRegistry()
    let bundle = KernelBridgeBundle(family: .relay)
    registry.register("relay_connector.tools", bundle)
    let kernel = Kernel(storage: .inMemory, bundles: registry)
    let root = Agent(id: "kernel_state", handlerModule: nil, parentId: nil)
    kernel.register(root)
    kernel.setRoot(root)
    _ = await kernel.send([
        Item(
            "kernel_state",
            .object([
                "type": .string("create_agent"), "parent_id": .string("kernel_state"),
                "handler_module": .string("relay_connector.tools"), "id": .string("brA"),
                "transport": .string("memory"),
            ]))
    ])[0].result
    return (kernel, bundle)
}

private func boot(_ kernel: Kernel, _ bundle: KernelBridgeBundle, _ wire: DirectoryWire) async {
    bundle.injectTransport(
        agentId: "brA", transport: await RelayTransport.attach(wire: wire, partnerGuid: "B"))
    _ = await kernel.send([Item("brA", ["type": "boot"])])[0].result
}

private func verb(_ kernel: Kernel, _ payload: JSON) async -> String {
    await kernel.send([Item("brA", payload)])[0].result.serialize()
}

private func teardown(_ kernel: Kernel) async {
    _ = await kernel.send([Item("kernel_state", ["type": "delete_agent", "id": "brA"])])[0].result
}

@Suite("relay_connector — the directory surface")
struct RelayDirectoryPinTests {

    @Test func beforeBootTheDirectoryVerbsAreNotConnected() async {
        let (kernel, _) = await legKernel()
        for name in ["list_peers", "watch_directory"] {
            #expect(
                await verb(kernel, .object(["type": .string(name)]))
                    == #"{"error":"relay_connector.\#(name): not connected","reason":"not_connected","#
                    + #""hint":"call boot first"}"#)
        }
        #expect(await verb(kernel, ["type": "unwatch_directory"]) == #"{"ok":true}"#)
    }

    /// A directory call goes to `target:"relay"` with a relay-level id, and the
    /// reply's `data` is the answer — `{}` when the relay sends none.
    @Test func listPeersAndWatchRoundTripTheRelaysReply() async {
        let (kernel, bundle) = await legKernel()
        let peers: JSON = .object([
            "peers": .array([.object(["guid": .string("B"), "status": .string("green")])])
        ])
        let wire = DirectoryWire(answer: peers)
        await boot(kernel, bundle, wire)
        #expect(await verb(kernel, ["type": "list_peers"]) == peers.serialize())
        #expect(await verb(kernel, ["type": "watch_directory"]) == peers.serialize())
        #expect(
            await verb(kernel, ["type": "unwatch_directory"]) == #"{"ok":true,"unwatched":"relay"}"#
        )
        let sent = await wire.sent
        #expect(
            sent == [
                // python `json.dumps` separators — the codec's text frame
                #"{"type": "call", "payload": {"type": "list_peers"}, "id": "dir_1", "target": "relay"}"#,
                #"{"type": "watch", "id": "dir_2", "target": "relay"}"#,
                #"{"type": "unwatch", "target": "relay"}"#,
            ], "\(sent)")
        await teardown(kernel)

        let (k2, b2) = await legKernel()
        await boot(k2, b2, DirectoryWire(answer: .null))
        #expect(await verb(k2, ["type": "list_peers"]) == "{}")
        await teardown(k2)
    }

    /// No reply within the bound: the request is removed and answered with
    /// python's text; the timeout is python's float repr.
    @Test func anUnansweredDirectoryCallTimesOutWithPythonsText() async {
        let (kernel, bundle) = await legKernel()
        await boot(kernel, bundle, DirectoryWire(answer: nil))
        #expect(
            await verb(kernel, ["type": "list_peers", "list_timeout": 0.2])
                == #"{"error":"relay_connector: directory timeout after 0.2s","reason":"timeout"}"#)
        await teardown(kernel)
    }

    /// A send that fails is a transport error naming the cause, and the socket
    /// is dropped (the leg reads disconnected).
    @Test func aFailedDirectorySendIsATransportError() async {
        let wire = DirectoryWire(failSends: true)
        let t = await RelayTransport.attach(wire: wire, partnerGuid: "B")
        #expect(await t.isLive)
        #expect(
            (await t.listPeers(timeout: 5)).serialize()
                == #"{"error":"relay_connector: directory send failed: wire is down","reason":"transport_error"}"#
        )
        #expect(await t.isLive == false)
        // with the socket gone the next request is not even attempted
        #expect(
            (await t.watchDirectory()).serialize()
                == #"{"error":"relay_connector: not connected","reason":"not_connected"}"#)
        #expect((await t.unwatchDirectory()).serialize() == #"{"ok":true}"#)
        await t.close()
    }

    /// Closing the transport settles every parked directory request ONCE with
    /// the closed refusal; its later timeout finds nothing to settle.
    @Test func closeSettlesEveryPendingDirectoryRequest() async {
        let t = await RelayTransport.attach(wire: DirectoryWire(answer: nil), partnerGuid: "B")
        async let a = t.listPeers(timeout: 0.5)
        async let b = t.watchDirectory(timeout: 0.5)
        try? await Task.sleep(nanoseconds: 100_000_000)
        await t.close()
        let (ra, rb) = await (a, b)
        let closed = #"{"error":"relay_connector: closed","reason":"transport_closed"}"#
        #expect(ra.serialize() == closed)
        #expect(rb.serialize() == closed)
        #expect(await t.isClosed())
        // outlive the timeouts: a second settle would crash on the continuation
        try? await Task.sleep(nanoseconds: 600_000_000)
        #expect(await t.isLive == false)
    }

    /// The relay reader skips what it cannot use — undecodable frames, non-object
    /// envelopes, frames from a peer that is not the partner, a stray reply —
    /// and hands the engine the next partner frame. A directory event arrives
    /// as a bare bridge `event`.
    @Test func theReaderSkipsUnusableFramesAndKeepsReading() async throws {
        let partner: JSON = .object([
            "type": .string("event"), "source": .string("B"),
            "payload": .object([
                "type": .string("reply"), "id": .string("x-1"), "data": .object([:]),
            ]),
        ])
        let directory: JSON = .object([
            "type": .string("event"), "source": .string("relay"),
            "payload": .object(["type": .string("peer_joined"), "guid": .string("C")]),
        ])
        let stranger: JSON = .object([
            "type": .string("event"), "source": .string("Z"),
            "payload": .object(["type": .string("call")]),
        ])
        let wire = DirectoryWire(script: [
            .text("{not json"),
            .text("[1]"),
            .binary([0x00, 0x00]),
            .text(#"{"type":"reply","id":"dir_99","data":{"x":1}}"#),
            .text(#"{"type":"event","source":"B","payload":"not an object"}"#),
            .text(stranger.serialize()),
            .text(#"{"type":"announce"}"#),
            .text(directory.serialize()),
            .text(partner.serialize()),
        ])
        let t = await RelayTransport.attach(wire: wire, partnerGuid: "B")
        let first = try await t.recv()
        #expect(
            first.frame.serialize()
                == #"{"type":"event","src":"relay","payload":{"type":"peer_joined","guid":"C"}}"#)
        #expect(first.isBinary == false && first.body.isEmpty)
        let second = try await t.recv()
        #expect(second.frame.serialize() == #"{"type":"reply","id":"x-1","data":{}}"#)
        await t.close()
        await #expect(throws: (any Error).self) { _ = try await t.recv() }
    }

    /// THROUGH THE ENGINE (audit W-10, 2026-10-03): the directory event reached
    /// the read loop with no `src`, and the engine re-emits only a subscribed
    /// `src` — so `watch_directory` delivered nothing. Now the leg subscribes to
    /// the directory (`src:"relay"`): nothing flows before `watch_directory` or
    /// after `unwatch_directory`, and in between every `peer_*` event lands on
    /// the connector's inbox. Python
    /// `test_watch_directory_re_emits_directory_events_on_the_connector_inbox`.
    @Test(.timeLimit(.minutes(1)))
    func watchDirectoryReEmitsDirectoryEventsOnTheConnectorInbox() async throws {
        let (kernel, bundle) = await legKernel()
        let wire = DirectoryWire(answer: ["ok": true])
        await boot(kernel, bundle, wire)
        let watcher = AgentId("dir_watcher")
        let inbox = kernel.ensureInbox(watcher)
        kernel.watch(src: "brA", watcher: watcher)
        let seen = SeenEvents()
        let reader = Task {
            for await ev in inbox where (ev.payload["type"].asString ?? "").hasPrefix("peer_") {
                seen.add(ev.payload.serialize())
            }
        }
        func directory(_ payload: JSON) async {
            await wire.deliver(
                .text(
                    JSON.object([
                        "type": .string("event"), "source": .string("relay"), "payload": payload,
                    ]).serialize()))
            try? await Task.sleep(nanoseconds: 150_000_000)
        }

        await directory(["type": "peer_left", "guid": "X"])
        #expect(seen.all == [], "an event before the subscription leaked")

        #expect(await verb(kernel, ["type": "watch_directory"]) == #"{"ok":true}"#)
        await directory(["type": "peer_joined", "guid": "C"])
        await directory(["type": "peer_status", "guid": "C", "status": "red"])
        #expect(
            seen.all == [
                #"{"type":"peer_joined","guid":"C","src":"relay"}"#,
                #"{"type":"peer_status","guid":"C","status":"red","src":"relay"}"#,
            ])

        #expect(
            await verb(kernel, ["type": "unwatch_directory"])
                == #"{"ok":true,"unwatched":"relay"}"#)
        await directory(["type": "peer_evicted", "guid": "C"])
        #expect(seen.all.count == 2, "an event after the unsubscribe leaked: \(seen.all)")
        reader.cancel()
        await teardown(kernel)
    }
}

/// The directory events a watcher of the leg saw, in order.
private final class SeenEvents: @unchecked Sendable {
    private let lock = NSLock()
    private var items: [String] = []
    func add(_ s: String) { lock.withLock { items.append(s) } }
    var all: [String] { lock.withLock { items } }
}

/// A kernel whose root carries guid `g1` and ONE relay leg `brA` with the given
/// extra record fields.
private func guidLegKernel(_ extra: OrderedDictionary<String, JSON>) async -> (
    Kernel, KernelBridgeBundle
) {
    let registry = BundleRegistry()
    let bundle = KernelBridgeBundle(family: .relay)
    registry.register("relay_connector.tools", bundle)
    let kernel = Kernel(storage: .inMemory, bundles: registry)
    let root = Agent(id: "kernel_state", handlerModule: nil, parentId: nil, meta: ["guid": "g1"])
    kernel.register(root)
    kernel.setRoot(root)
    var create: OrderedDictionary<String, JSON> = [
        "type": "create_agent", "parent_id": "kernel_state",
        "handler_module": "relay_connector.tools", "id": "brA", "transport": "memory",
    ]
    for (k, v) in extra { create[k] = v }
    _ = await kernel.send([Item("kernel_state", .object(create))])[0].result
    return (kernel, bundle)
}

private func binaryForward(_ kernel: Kernel, _ header: JSON) async -> (String, Int) {
    // the binary channel with no request body: the forward's own refusals, before
    // any frame is built
    let out = await kernel.send([Item("brA", header, rawReply: true)])[0]
    return (out.result.serialize(), out.body?.bytes.count ?? 0)
}

@Suite("bridge engine — binary forward and inbound refusals")
struct BridgeEnginePinTests {

    /// The BINARY `forward` answers the text forward's texts, with an empty body.
    @Test func binaryForwardRefusals() async {
        let (kernel, bundle) = await guidLegKernel([:])
        let cold = await binaryForward(
            kernel, ["type": "forward", "target": "x", "payload": ["type": "ping"]])
        #expect(
            cold.0
                == #"{"error":"bridge.forward: not connected","reason":"not_connected","hint":"call boot first"}"#
        )
        #expect(cold.1 == 0)
        await boot(kernel, bundle, DirectoryWire(answer: nil))
        let required = #"{"error":"bridge.forward: target (str) + payload (dict) required"}"#
        #expect(
            await binaryForward(kernel, ["type": "forward", "payload": ["type": "ping"]]).0
                == required)
        #expect(
            await binaryForward(kernel, ["type": "forward", "target": "x", "payload": "ping"]).0
                == required)
        // nobody answers on the partner side: the bound is the reply
        let late = await binaryForward(
            kernel, ["type": "forward", "target": "x", "payload": ["type": "ping"], "forward_timeout": 0.2])
        #expect(late.0 == #"{"error":"bridge.forward: timeout after 0.2s"}"#)
        #expect(late.1 == 0)
        await teardown(kernel)
    }
}
