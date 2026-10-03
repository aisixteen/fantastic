// A bridge leg across its LIFETIME: a live record change, a sender restart.
//
// Audit 2026-10-03:
//
//   - ARCH-2 — the leg's rules were resolved ONCE at boot and cached, so an
//     operator's `update_agent ingress_rule=deny_ingress` on a LIVE leg changed
//     what `reflect` reported (sealed) and nothing the gate did: inbound calls
//     kept being dispatched until the transport was torn down. The opening
//     direction (the readme's own `update_agent <id> ingress_rule=allow_all_ingress`)
//     failed the same way. Now the leg reads its rules off the record per frame —
//     the record is the truth the moment it is written.
//   - IO-5 — a forward's frame id names the execution on the receiver, and it was
//     `<leg>-<n>` with `n` counting from 1 in every process. A restarted sender's
//     first forward reused `<leg>-1`, which the long-lived receiver had already
//     settled: the new write was JOINED to the old one, answered with its
//     recorded outcome, and never ran. Now every boot mints a nonce into the ids.
//
// Twin of python `ws_bridge/tests/test_leg_lifetime.py`.

import FantasticFile
import FantasticIoBridge
import FantasticJSON
import FantasticKernel
import FantasticKernelBridge
import FantasticKernelState
import Foundation
import OrderedCollections
import Testing

private func op(_ k: Kernel, _ target: String, _ payload: JSON) async -> JSON {
    await k.send([Item(AgentId(target), payload)])[0].result
}

private func tmpDir(_ tag: String) -> URL {
    let d = FileManager.default.temporaryDirectory
        .appendingPathComponent("leg-\(tag)-\(UUID().uuidString.prefix(8))")
    try? FileManager.default.createDirectory(at: d, withIntermediateDirectories: true)
    return d
}

/// A kernel with the loader root (guid `guid`, running dir `dir`), file bridges
/// and a ws_bridge bundle — the bundle handed back so a test injects transports.
private func kernel(guid: String, dir: URL) -> (Kernel, KernelBridgeBundle) {
    let ws = KernelBridgeBundle(family: .ws)
    let r = BundleRegistry()
    r.register(ROOT_HANDLER_MODULE, KernelStateBundle())
    r.register("file_bridge.tools", FileBundle())
    r.register("ws_bridge.tools", ws)
    let k = Kernel(storage: .inMemory, bundles: r)
    let root = Agent(
        id: AgentId("kernel_state"), handlerModule: ROOT_HANDLER_MODULE, parentId: nil,
        rootPath: dir.appendingPathComponent(".fantastic"))
    k.register(root)
    k.setRoot(root)
    k.update(root.id, ["guid": .string(guid)])
    return (k, ws)
}

private func create(_ k: Kernel, _ fields: OrderedDictionary<String, JSON>) async -> String {
    var p: OrderedDictionary<String, JSON> = ["type": "create_agent"]
    for (key, v) in fields { p[key] = v }
    let out = await op(k, "kernel", .object(p))
    #expect(out["id"].asString != nil, "\(out.serialize())")
    return out["id"].asString ?? ""
}

/// Python fixture `leg`: one leg on `k`, pinned to `peer-g`, its raw peer end in
/// hand; `notes` (a child of the leg) accepts the foreign caller `peer-g|ai`.
private func leg() async -> (Kernel, String, PairEnd, URL) {
    let dir = tmpDir("arch2")
    let (k, ws) = kernel(guid: "local-g", dir: dir)
    let legId = await create(
        k,
        [
            "parent_id": "kernel_state", "handler_module": "ws_bridge.tools",
            "transport": "memory", "peer_guid": "peer-g", "ingress_rule": "allow_all_ingress",
        ])
    let notes = await create(
        k,
        [
            "parent_id": .string(legId), "handler_module": "file_bridge.tools", "id": "notes",
            "callable_by": ["peer-g|ai": "*"],
        ])
    #expect(notes == "notes")
    let (mine, peer) = PairEnd.pair()
    ws.injectTransport(agentId: AgentId(legId), transport: mine)
    let booted = await op(k, legId, ["type": "boot"])
    #expect(booted["booted"] == true, "\(booted.serialize())")
    return (k, legId, peer, dir)
}

/// One inbound `reflect` of `notes` from `peer-g|ai`; the reply's `data`.
private func inbound(_ peer: PairEnd, _ n: Int) async throws -> JSON {
    try await peer.send(
        [
            "type": "call", "id": .string("peer-call-\(n)"), "target": "notes",
            "payload": ["type": "reflect"], "caller": "peer-g|ai",
        ])
    return try await peer.recv().frame["data"]
}

/// Run one forward and hand back the frame that went on the wire.
private func forwardFrame(_ k: Kernel, _ legId: String, _ peer: PairEnd) async throws -> JSON {
    let sent = Task {
        await op(
            k, legId, ["type": "forward", "target": "x", "payload": ["type": "reflect"]])
    }
    let frame = try await peer.recv().frame
    try await peer.send(["type": "reply", "id": frame["id"], "data": ["ok": true]])
    #expect(await sent.value == ["ok": true])
    return frame
}

@Suite("bridge leg lifetime — live rules, per-boot frame ids")
struct LegLifetimeTests {

    @Test(.timeLimit(.minutes(1)))
    func resealingALiveLegRefusesTheNextInboundCall() async throws {
        let (k, legId, peer, dir) = await leg()
        defer { try? FileManager.default.removeItem(at: dir) }
        #expect(try await inbound(peer, 1)["id"] == "notes")
        let sealed = await op(
            k, "kernel", ["type": "update_agent", "id": .string(legId), "ingress_rule": "deny_ingress"])
        #expect(sealed["updated"] == true, "\(sealed.serialize())")
        let shown = await op(k, legId, ["type": "reflect"])
        #expect(shown["sealed"] == true, "\(shown.serialize())")
        let out = try await inbound(peer, 2)
        #expect(out["reason"] == "unauthorized", "\(out.serialize())")
        #expect(out["id"] == .null, "a sealed leg dispatches nothing")
    }

    @Test(.timeLimit(.minutes(1)))
    func openingALiveSealedLegAdmitsTheNextInboundCall() async throws {
        let (k, legId, peer, dir) = await leg()
        defer { try? FileManager.default.removeItem(at: dir) }
        _ = await op(
            k, "kernel", ["type": "update_agent", "id": .string(legId), "ingress_rule": "deny_ingress"])
        #expect(try await inbound(peer, 1)["reason"] == "unauthorized")
        let opened = await op(
            k, "kernel",
            ["type": "update_agent", "id": .string(legId), "ingress_rule": "allow_all_ingress"])
        #expect(opened["updated"] == true, "\(opened.serialize())")
        #expect(try await inbound(peer, 2)["id"] == "notes")
    }

    @Test(.timeLimit(.minutes(1)))
    func aLiveEgressChangeStampsTheNextFrame() async throws {
        let env = "FANTASTIC_GROUP_TOKEN_LEG_EGRESS"
        setenv(env, "grp-1", 1)
        defer { unsetenv(env) }
        let (k, legId, peer, dir) = await leg()
        defer { try? FileManager.default.removeItem(at: dir) }
        #expect(try await forwardFrame(k, legId, peer)["auth_token"] == .null)
        _ = await op(
            k, "kernel",
            [
                "type": "update_agent", "id": .string(legId),
                "egress_rule": ["type": "password_egress", "env": .string(env)],
            ])
        #expect(try await forwardFrame(k, legId, peer)["auth_token"] == "grp-1")
        _ = await op(
            k, "kernel", ["type": "update_agent", "id": .string(legId), "egress_rule": "silent_egress"])
        #expect(try await forwardFrame(k, legId, peer)["auth_token"] == .null)
    }

    /// An egress rule is resolved per forward now, so one the registry does not
    /// know refuses that forward, loud — nothing goes on the wire.
    @Test(.timeLimit(.minutes(1)))
    func anUnknownEgressRuleWrittenLiveRefusesTheForward() async throws {
        let (k, legId, _, dir) = await leg()
        defer { try? FileManager.default.removeItem(at: dir) }
        _ = await op(
            k, "kernel", ["type": "update_agent", "id": .string(legId), "egress_rule": "nope_egress"])
        let out = await op(
            k, legId, ["type": "forward", "target": "x", "payload": ["type": "reflect"]])
        #expect(
            out == ["error": "bridge: bad auth rule: unknown egress rule type 'nope_egress'"],
            "\(out.serialize())")
    }

    // ─── IO-5: a restarted sender never joins a stale execution ─────────────

    /// The SENDER restarts (a new process: a new kernel and a new bundle, the
    /// same leg id and guid) while the receiver keeps running, every settled
    /// execution still named.
    @Test(.timeLimit(.minutes(1)))
    func aRestartedSenderNeverJoinsASettledExecution() async throws {
        let dirB = tmpDir("io5-b")
        defer { try? FileManager.default.removeItem(at: dirB) }
        let (kb, wsB) = kernel(guid: "guid-b", dir: dirB)
        let bId = await create(
            kb,
            [
                "parent_id": "kernel_state", "handler_module": "ws_bridge.tools",
                "id": "b_leg", "transport": "memory", "ingress_rule": "allow_all_ingress",
                "peer_guid": "guid-a",
            ])
        let doc = await create(
            kb,
            [
                "parent_id": .string(bId), "handler_module": "file_bridge.tools", "id": "doc",
                "allow": ["."], "ingress_rule": "allow_all_ingress",
                "callable_by": ["guid-a|*": "*"],
            ])
        #expect(doc == "doc")

        /// One sender process: a fresh kernel A paired with B's leg (reconnected).
        func sender(_ tag: String) async -> (Kernel, URL) {
            let dirA = tmpDir("io5-a-\(tag)")
            let (ka, wsA) = kernel(guid: "guid-a", dir: dirA)
            let aId = await create(
                ka,
                [
                    "parent_id": "kernel_state", "handler_module": "ws_bridge.tools",
                    "id": "a_leg", "transport": "memory", "peer_guid": "guid-b",
                ])
            let (ta, tb) = PairEnd.pair()
            wsA.injectTransport(agentId: AgentId(aId), transport: ta)
            wsB.injectTransport(agentId: AgentId(bId), transport: tb)
            for (k, id) in [(ka, aId), (kb, bId)] {
                let r = await op(k, id, ["type": "reconnect"])
                #expect(r["booted"] == true, "\(r.serialize())")
            }
            return (ka, dirA)
        }
        func write(_ ka: Kernel, _ text: String) async -> JSON {
            await op(
                ka, "a_leg",
                [
                    "type": "forward", "target": "doc",
                    "payload": ["type": "write", "path": "doc.txt", "bytes": .string(text)],
                ])
        }

        let (a1, dirA1) = await sender("1")
        defer { try? FileManager.default.removeItem(at: dirA1) }
        let first = await write(a1, "v1")
        #expect(first["size"] == 2, "\(first.serialize())")
        _ = await op(a1, "kernel", ["type": "delete_agent", "id": "a_leg"])  // the process ends

        let (a2, dirA2) = await sender("2")
        defer { try? FileManager.default.removeItem(at: dirA2) }
        let second = await write(a2, "v2-NEW")
        #expect(second["size"] == 6, "\(second.serialize())")
        let read = await op(kb, "doc", ["type": "read", "path": "doc.txt"])
        #expect(
            (read["lines"].asArray ?? []).map { $0["text"] } == ["v2-NEW"], "\(read.serialize())")
    }
}
