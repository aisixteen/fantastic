// CROSS-KERNEL ADDRESSING (owner, 2026-09-30): an agent on a connected peer
// kernel is addressed `<guid>|<agent id>`, called with an ordinary `send`, and
// discovered through reflect. The KERNEL routes it — a transport only registers
// the peer it is connected to (`registerPeer`) and carries the frames.
//
// The twin of python `tests/kernel/test_peer_addressing.py`, driving the kernel
// against a fake route; the web ws leg's real route is covered in
// `FantasticBundlesTests/PeerCallsTests.swift`.

import FantasticJSON
import Foundation
import OrderedCollections
import Testing

@testable import FantasticKernel

private let HOST = "h0"  // this kernel's guid, set by hand: the root is not booted here
private let PAGE = "g1"

/// A local agent that answers every verb with its own id.
private struct Echo: AgentBundle {
    let name = "echo"
    func handle(agentId: AgentId, payload: JSON, kernel: Kernel) async throws -> JSON? {
        .object(["id": .string(agentId.value)])
    }
}

private func mkKernel(guid: String?) -> Kernel {
    let registry = BundleRegistry()
    registry.register("echo.tools", Echo())
    let kernel = Kernel(storage: .inMemory, bundles: registry)
    var meta: OrderedDictionary<String, JSON> = [:]
    if let guid { meta["guid"] = .string(guid) }
    let root = Agent(id: "kernel_state", handlerModule: nil, parentId: nil, meta: meta)
    kernel.register(root)
    kernel.setRoot(root)
    return kernel
}

/// Every call a fake route delivered: `(agent id, payload, caller)`, and the
/// `secret` marks each one carried.
private final class Calls: @unchecked Sendable {
    private let lock = NSLock()
    private var items: [(String, JSON, String)] = []
    private var marks: [[String]] = []
    func add(_ c: (String, JSON, String), _ secret: [String]) {
        lock.withLock {
            items.append(c)
            marks.append(secret)
        }
    }
    var all: [(String, JSON, String)] { lock.withLock { items } }
    var secrets: [[String]] { lock.withLock { marks } }
}

/// A fake route that records every call and answers `reply` (or `{ok: agent}`).
private func route(_ via: String, _ calls: Calls, reply: JSON? = nil) -> PeerRoute {
    PeerRoute(via: via) { agentId, payload, caller, secret, _ in
        calls.add((agentId, payload, caller), secret)
        return (reply ?? .object(["ok": .string(agentId)]), nil)
    }
}

@discardableResult
private func connect(_ k: Kernel, _ calls: Calls, reply: JSON? = nil) -> PeerRoute {
    let r = route("leg", calls, reply: reply)
    #expect(k.registerPeer(PAGE, r) == nil)
    return r
}

private func send(_ k: Kernel, _ target: String, _ payload: JSON, as sender: String? = nil) async
    -> Outcome
{
    let items = [Item(AgentId(target), payload)]
    guard let sender else { return await k.send(items)[0] }
    return await KernelTaskLocals.$currentSender.withValue(AgentId(sender)) {
        await k.send(items)[0]
    }
}

private func create(_ k: Kernel, _ id: String, canCall: JSON? = nil) async {
    var p: OrderedDictionary<String, JSON> = [
        "type": .string("create_agent"), "parent_id": .string("kernel"), "id": .string(id),
        "handler_module": .string("echo.tools"),
    ]
    if let canCall { p["can_call"] = canCall }
    let r = await k.send([Item(AgentId("kernel"), .object(p))])[0].result
    #expect(r["error"].isNull, "\(r.serialize())")
}

private func verbs(_ v: String...) -> JSON { .array(v.map { .string($0) }) }

private func type(_ verb: String) -> JSON { .object(["type": .string(verb)]) }

@Suite("cross-kernel addressing")
struct PeerAddressingTests {
    @Test func anOperatorSendReachesThePeerAgentAsThisKernel() async {
        let k = mkKernel(guid: HOST)
        let calls = Calls()
        connect(k, calls, reply: .object(["cells": .array([.integer(1), .integer(2)])]))
        let payload: JSON = .object([
            "type": .string("around"), "x": .integer(1), "y": .integer(2), "radius": .integer(3),
        ])
        let out = await send(k, "\(PAGE)|canvas", payload)
        #expect(out.status == .completed, "\(out.result.serialize())")
        #expect(out.result == .object(["cells": .array([.integer(1), .integer(2)])]))
        #expect(calls.all.count == 1)
        #expect(calls.all.first?.0 == "canvas")
        #expect(calls.all.first?.1 == payload)
        #expect(calls.all.first?.2 == "\(HOST)|kernel")
    }

    /// The route gets the item's `secret` paths verbatim (rooted at the item, as
    /// the peer's item is), so the PEER masks its records; this kernel masks its
    /// own.
    @Test func theItemsSecretMarksRideToThePeer() async {
        let k = mkKernel(guid: HOST)
        let calls = Calls()
        connect(k, calls, reply: .object(["ok": .bool(true)]))
        let key = "sk-peer-0123456789"
        let payload: JSON = .object([
            "type": .string("set"), "fields": .object(["api_key": .string(key)]),
        ])
        var item = Item(AgentId("\(PAGE)|creds"), payload)
        item.secret = .array([.string("payload.fields"), .string("result")])
        let out = await k.send([item])[0]
        #expect(out.status == .completed, "\(out.result.serialize())")
        #expect(calls.all.count == 1)
        #expect(calls.all.first?.0 == "creds")
        #expect(calls.all.first?.1 == payload)
        #expect(calls.all.first?.2 == "\(HOST)|kernel")
        #expect(calls.secrets == [["payload.fields", "result"]], "\(calls.secrets)")
        #expect(
            k.rows.getOne(out.pid!.value)?["payload"]?["fields"] == .string(Secret.placeholder),
            "\(k.rows.getOne(out.pid!.value).map { JSON.object($0).serialize() } ?? "nil")")
        // an unmarked call hands the route no marks
        _ = await send(k, "\(PAGE)|creds", type("reflect"))
        #expect(calls.secrets.last == [], "\(calls.secrets)")
    }

    @Test func anAgentNeedsTheQualifiedTargetInItsCanCall() async {
        let k = mkKernel(guid: HOST)
        let calls = Calls()
        connect(k, calls)
        let canvas = "\(PAGE)|canvas"
        await create(k, "plain")
        await create(k, "named", canCall: .object([canvas: verbs("around")]))
        await create(k, "wild", canCall: .object(["\(PAGE)|*": verbs("around")]))
        let refused = await send(k, canvas, type("around"), as: "plain")
        let r = refused.result
        #expect(r["reason"].asString == "unauthorized", "\(r.serialize())")
        #expect(r["side"].asString == "egress", "\(r.serialize())")
        #expect(r["hint"].asString?.contains("\(PAGE)|*") == true, "\(r.serialize())")
        // a literal grant and the whole-kernel `<guid>|*` both reach it
        for sender in ["named", "wild"] {
            let ok = await send(k, canvas, type("around"), as: sender)
            #expect(ok.status == .completed, "\(sender): \(ok.result.serialize())")
        }
        #expect(calls.all.map { $0.2 } == ["\(HOST)|named", "\(HOST)|wild"])
        // a verb the grant does not name is refused at egress too
        let denied = await send(k, canvas, type("clear"), as: "named")
        #expect(denied.result["side"].asString == "egress", "\(denied.result.serialize())")
    }

    @Test func aGuidNobodyIsConnectedAsIsAnUnknownAgent() async {
        let k = mkKernel(guid: HOST)
        let out = await send(k, "nobody|canvas", type("reflect"))
        #expect(out.result["reason"].asString == "unknown_agent", "\(out.result.serialize())")
    }

    @Test func thisKernelsOwnGuidNamesALocalAgent() async {
        let k = mkKernel(guid: HOST)
        await create(k, "cli")
        let out = await send(k, "\(HOST)|cli", type("reflect"))
        #expect(out.status == .completed, "\(out.result.serialize())")
        #expect(out.result["id"].asString == "cli", "\(out.result.serialize())")
        let root = await send(
            k, "\(HOST)|kernel", .object(["type": .string("reflect"), "tree": .string("none")]))
        #expect(root.result["id"].asString == "kernel_state", "\(root.result.serialize())")
    }

    @Test func theRootReflectListsConnectedPeers() async {
        let k = mkKernel(guid: HOST)
        let r = connect(k, Calls())
        let reflect: JSON = .object(["type": .string("reflect"), "tree": .string("none")])
        let out = await send(k, "kernel", reflect)
        #expect(
            out.result["peers"]
                == .array([.object(["guid": .string(PAGE), "via": .string("leg")])]),
            "\(out.result.serialize())")
        // after `runtime`, python's key order
        let keys = Array(out.result.asObject?.keys ?? [])
        let at = keys.firstIndex(of: "runtime")
        #expect(at != nil && keys[at! + 1] == "peers", "\(keys)")
        k.unregisterPeer(PAGE, r)
        let after = await send(k, "kernel", reflect)
        #expect(after.result["peers"] == .array([]), "\(after.result.serialize())")
    }

    @Test func oneGuidOneLiveRoute() async {
        let k = mkKernel(guid: HOST)
        let calls = Calls()
        let first = connect(k, calls)
        let other = route("leg2", calls)
        let refused = k.registerPeer(PAGE, other)
        #expect(
            refused?["error"].asString?.contains("already connected via leg") == true,
            "\(refused?.serialize() ?? "nil")")
        #expect(refused?["arg"].asString == "guid")
        // only the registered route may unregister it
        k.unregisterPeer(PAGE, other)
        #expect(k.peer(PAGE) != nil)
        k.unregisterPeer(PAGE, first)
        #expect(k.peer(PAGE) == nil)
    }

    @Test func aKernelWithNoGuidCannotSayWhoIsCalling() async {
        let k = mkKernel(guid: nil)
        let calls = Calls()
        connect(k, calls)
        let out = await send(k, "\(PAGE)|canvas", type("around"))
        #expect(
            out.result["error"].asString?.contains("no guid yet") == true,
            "\(out.result.serialize())")
        #expect(calls.all.isEmpty)
    }

    /// Discovery by reflect: the peer's reply comes back as it answered it — it
    /// is NOT re-rendered here (there is no local agent to render it from).
    @Test func aReflectOfAPeerReturnsThatKernelsOwnAnswer() async {
        let k = mkKernel(guid: HOST)
        let calls = Calls()
        let theirs: JSON = .object([
            "id": .string("kernel_state"),
            "tree": .array([.object(["id": .string("canvas")])]),
        ])
        connect(k, calls, reply: theirs)
        let out = await send(k, "\(PAGE)|kernel", type("reflect"))
        #expect(out.status == .completed, "\(out.result.serialize())")
        #expect(out.result == theirs, "\(out.result.serialize())")
        #expect(calls.all.first?.0 == "kernel")
        #expect(calls.all.first?.1 == type("reflect"))
    }

    /// `create_agent` sent to `<guid>|kernel` places nothing in THIS tree: it is
    /// an ordinary call to the peer, gated on egress like any other — the local
    /// placement rule (a child of yourself is baseline) must not wave it through.
    @Test func aCreateOnAPeerIsGatedOnThePeerTarget() async {
        let k = mkKernel(guid: HOST)
        let calls = Calls()
        connect(k, calls)
        await create(k, "maker")
        let out = await send(
            k, "\(PAGE)|kernel",
            .object([
                "type": .string("create_agent"), "parent_id": .string("maker"),
                "handler_module": .string("x.tools"),
            ]),
            as: "maker")
        #expect(out.result["side"].asString == "egress", "\(out.result.serialize())")
        #expect(calls.all.isEmpty)
    }
}
