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

/// Every call a fake route delivered: `(agent id, payload)`. A frame names no
/// caller: the peer judges what arrives as the leg that serves it there.
private final class Calls: @unchecked Sendable {
    private let lock = NSLock()
    private var items: [(String, JSON)] = []
    func add(_ c: (String, JSON)) {
        lock.withLock { items.append(c) }
    }
    var all: [(String, JSON)] { lock.withLock { items } }
}

/// A fake route that records every call and answers `reply` (or `{ok: agent}`).
private func route(_ via: String, _ calls: Calls, reply: JSON? = nil) -> PeerRoute {
    PeerRoute(via: via) { agentId, payload, _ in
        calls.add((agentId, payload))
        return (reply ?? .object(["ok": .string(agentId)]), nil)
    } emit: { agentId, payload, _ in
        calls.add(("emit:\(agentId)", payload))
    }
}

@discardableResult
private func connect(_ k: Kernel, _ calls: Calls, reply: JSON? = nil) -> PeerRoute {
    let r = route("leg", calls, reply: reply)
    k.registerPeer(PAGE, r)
    return r
}

/// One TAB of the identity PAGE: its route records the call as `<name>:<agent>`
/// and answers when `answer` is called — or `{cancelled}` when its call is
/// cancelled first (`cancelled` says so).
private final class Tab: @unchecked Sendable {
    private let lock = NSLock()
    private var reply: JSON?
    private var waiter: CheckedContinuation<JSON, Never>?
    private var wasCancelled = false
    var cancelled: Bool { lock.withLock { wasCancelled } }

    func answer(_ r: JSON) {
        let w: CheckedContinuation<JSON, Never>? = lock.withLock {
            if let w = waiter {
                waiter = nil
                return w
            }
            reply = r
            return nil
        }
        w?.resume(returning: r)
    }

    func route(_ name: String, _ calls: Calls) -> PeerRoute {
        PeerRoute(via: "leg_\(name)") { agentId, payload, _ in
            calls.add(("\(name):\(agentId)", payload))
            let r = await withTaskCancellationHandler {
                await withCheckedContinuation { (c: CheckedContinuation<JSON, Never>) in
                    let ready: JSON? = self.lock.withLock {
                        if let r = self.reply { return r }
                        if self.wasCancelled { return .object(["cancelled": .bool(true)]) }
                        self.waiter = c
                        return nil
                    }
                    if let ready { c.resume(returning: ready) }
                }
            } onCancel: {
                let w: CheckedContinuation<JSON, Never>? = self.lock.withLock {
                    self.wasCancelled = true
                    defer { self.waiter = nil }
                    return self.waiter
                }
                w?.resume(returning: .object(["cancelled": .bool(true)]))
            }
            return (r, nil)
        } emit: { agentId, payload, _ in
            calls.add(("\(name):emit:\(agentId)", payload))
        }
    }
}

private func send(_ k: Kernel, _ target: String, _ payload: JSON, as sender: String? = nil) async
    -> Outcome
{
    let items = [Item(AgentId(target), payload)]
    guard let sender else { return await k.send(items)[0] }
    return await KernelTaskLocals.$sender.withValue(AgentId(sender)) {
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
    @Test func anOperatorSendReachesThePeerAgent() async {
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
        #expect(calls.all.map { $0.0 } == ["canvas", "canvas"])
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

    /// The tabs of one identity are ONE principal (owner, 2026-10-03): a second
    /// route for a live guid is ADDED, and a disconnect removes only its own.
    @Test func oneGuidHoldsASetOfLiveRoutes() async {
        let k = mkKernel(guid: HOST)
        let calls = Calls()
        let first = connect(k, calls)
        let other = route("leg2", calls)
        k.registerPeer(PAGE, other)
        k.registerPeer(PAGE, other)  // the same route twice stays one
        #expect(
            k.peerRows()
                == .array([
                    .object(["guid": .string(PAGE), "via": .string("leg")]),
                    .object(["guid": .string(PAGE), "via": .string("leg2")]),
                ]))
        k.unregisterPeer(PAGE, other)
        let left = k.peerRoutes(PAGE)
        #expect(left.count == 1 && left[0] === first)
        k.unregisterPeer(PAGE, first)
        #expect(k.peerRoutes(PAGE).isEmpty)
        #expect(k.peerRows() == .array([]))
    }

    @Test(.timeLimit(.minutes(1)))
    func aCallReachesEveryTabAndSettlesWithTheFirstReply() async {
        let k = mkKernel(guid: HOST)
        let calls = Calls()
        let a = Tab(), b = Tab()
        k.registerPeer(PAGE, a.route("a", calls))
        k.registerPeer(PAGE, b.route("b", calls))
        let send = Task { await send(k, "\(PAGE)|canvas", type("around")) }
        while calls.all.count < 2 { await Task.yield() }
        let seen = calls.all.sorted { $0.0 < $1.0 }
        #expect(seen.map(\.0) == ["a:canvas", "b:canvas"])
        #expect(seen.allSatisfy { $0.1 == type("around") })
        b.answer(.object(["from": .string("b")]))
        let out = await send.value
        #expect(out.status == .completed, "\(out.result.serialize())")
        #expect(out.result == .object(["from": .string("b")]))
        // the slower tab's call is cancelled, not left waiting
        #expect(a.cancelled)
    }

    /// MULTI-TAB EMIT: an emit to `<guid>|<agent>` reaches EVERY live tab of the
    /// identity — an operator emit, and an agent's once its `can_call` names the
    /// target.
    @Test func anEmitReachesEveryTabOfTheIdentity() async {
        let k = mkKernel(guid: HOST)
        let calls = Calls()
        let a = Tab(), b = Tab()
        k.registerPeer(PAGE, a.route("a", calls))
        k.registerPeer(PAGE, b.route("b", calls))
        await k.emit(AgentId("\(PAGE)|canvas"), type("tick"))
        #expect(calls.all.map(\.0) == ["a:emit:canvas", "b:emit:canvas"])
        #expect(calls.all.allSatisfy { $0.1 == type("tick") })
        await create(k, "lonely")
        await create(k, "speaker", canCall: .object(["\(PAGE)|canvas": verbs("emit")]))
        await KernelTaskLocals.$sender.withValue(AgentId("lonely")) {
            await k.emit(AgentId("\(PAGE)|canvas"), type("denied"))
        }
        await KernelTaskLocals.$sender.withValue(AgentId("speaker")) {
            await k.emit(AgentId("\(PAGE)|canvas"), type("said"))
        }
        let rest = Array(calls.all.dropFirst(2))
        #expect(rest.map(\.0) == ["a:emit:canvas", "b:emit:canvas"])
        #expect(rest.allSatisfy { $0.1 == type("said") })
    }

    @Test func aDisconnectedTabLeavesTheIdentityRoutable() async {
        let k = mkKernel(guid: HOST)
        let calls = Calls()
        let a = Tab(), b = Tab()
        let gone = a.route("a", calls)
        k.registerPeer(PAGE, gone)
        k.registerPeer(PAGE, b.route("b", calls))
        b.answer(.object(["from": .string("b")]))
        k.unregisterPeer(PAGE, gone)
        let out = await send(k, "\(PAGE)|canvas", type("around"))
        #expect(out.result == .object(["from": .string("b")]), "\(out.result.serialize())")
        #expect(calls.all.map(\.0) == ["b:canvas"])
    }

    /// The frame names no caller: the peer judges the call as the leg that
    /// serves it there, so this kernel's own guid is not part of the call.
    @Test func aCallToAPeerNeedsNoGuidOfThisKernel() async {
        let k = mkKernel(guid: nil)
        let calls = Calls()
        connect(k, calls)
        let out = await send(k, "\(PAGE)|canvas", type("around"))
        #expect(out.status == .completed, "\(out.result.serialize())")
        #expect(calls.all.count == 1 && calls.all[0].0 == "canvas" && calls.all[0].1 == type("around"))
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
    /// mutator rule (a child of yourself) must not wave it through.
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
