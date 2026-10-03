// JOINING a secret-marked execution (audit 2026-10-03, F1).
//
// A join hands back the execution's outcome without running anything, re-gated
// on the ORIGINAL verb. Measured before the fix: `eve`, granted the same verb on a
// credential_store but never a reader of the ref, joined `alice`'s marked `get`
// by its int pid (and by its name) and received the real key — the store's
// per-ref `readers` check runs in its handler, which a joiner never reaches, and
// the joiner marked nothing.
//
// The rule: an execution whose item carried `secret` marks hands its REAL outcome
// to its ORIGINAL caller only — the identity that opened it: the sending agent,
// or the operator (no sender), or, when a bridge leg relays a foreign caller,
// that caller's qualified `<guid>|<agent>`. Every other joiner gets the outcome AS
// SHOWN — the result marks and every secret value replaced by `<secret>`, exactly
// what the ledger reads back. A join of an unmarked execution is unchanged.
//
// Twin of python `tests/kernel/test_secret_join.py`.

import FantasticJSON
import Foundation
import OrderedCollections
import Testing

@testable import FantasticKernel

private let KEY = "sk-join-0123456789abcdef"
private let MOD = "secret_join_probe.tools"

/// A latch the in-flight test opens (python's `_release` event).
private final class Latch: @unchecked Sendable {
    private let lock = NSLock()
    private var open = false
    func release() { lock.withLock { open = true } }
    var isOpen: Bool { lock.withLock { open } }
}

private struct Probe: AgentBundle {
    let name = "secret_join_probe"
    let latch: Latch
    func handle(agentId: AgentId, payload: JSON, kernel: Kernel) async throws -> JSON? {
        switch payload["type"].asString ?? "" {
        case "boot", "shutdown", "reflect":
            return .object([
                "id": .string(agentId.value), "sentence": "join probe", "verbs": .object([:]),
            ])
        case "issue":
            return .object(["key": .string(KEY), "note": "issued"])
        case "slow_issue":
            while !latch.isOpen { try? await Task.sleep(nanoseconds: 1_000_000) }
            return .object(["key": .string(KEY), "note": "issued"])
        case "echo":
            return .object(["echo": payload["text"]])
        default:
            return .object(["error": .string("unknown verb \(pyRepr(payload["type"]))")])
        }
    }
}

private func seeded(_ latch: Latch = Latch()) async -> Kernel {
    let r = BundleRegistry()
    r.register(MOD, Probe(latch: latch))
    let k = Kernel(storage: .inMemory, bundles: r)
    let root = Agent(id: "kernel_state", handlerModule: nil, parentId: nil, meta: ["guid": "g1"])
    k.register(root)
    k.setRoot(root)
    func create(_ meta: OrderedDictionary<String, JSON>) async {
        var p: OrderedDictionary<String, JSON> = [
            "type": "create_agent", "parent_id": "kernel_state",
        ]
        for (key, v) in meta { p[key] = v }
        let out = await k.send([Item("kernel", .object(p))])[0].result
        #expect(out["id"] == p["id"], "\(out.serialize())")
    }
    await create([
        "id": "vault", "handler_module": .string(MOD),
        "callable_by": ["alice": "*", "eve": "*", "leg": "*", "P-g|*": "*"],
    ])
    for aid in ["alice", "eve", "leg"] {
        // swift's create_agent needs a handler_module (python's fixture calls
        // `create(None, …)` directly); the probe answers nothing they are asked
        await create(["id": .string(aid), "handler_module": .string(MOD), "can_call": ["vault": "*"]])
    }
    return k
}

private func send(
    _ k: Kernel, _ item: Item, _ sender: String? = nil, remote: String? = nil
) async -> Outcome {
    guard let sender else { return await k.send([item])[0] }
    return await KernelTaskLocals.$currentSender.withValue(AgentId(sender)) {
        guard let remote else { return await k.send([item])[0] }
        return await KernelTaskLocals.withRemoteCaller(leg: AgentId(sender), address: remote) {
            await k.send([item])[0]
        }
    }
}

private func issue(id: String? = nil, verb: String = "issue") -> Item {
    var it = Item("vault", .object(["type": .string(verb)]), id: id)
    it.secret = ["result.key"]
    return it
}

private func join(_ id: JSON, _ payload: JSON = ["type": "issue"]) -> Item {
    var it = Item("vault", payload)
    it.rawId = id
    return it
}

private func isShown(_ o: Outcome) -> Bool {
    o.status == .completed && o.result == ["key": .string(Secret.placeholder), "note": "issued"]
}

private func isReal(_ o: Outcome) -> Bool {
    o.status == .completed && o.result == ["key": .string(KEY), "note": "issued"]
}

@Suite("joining a secret-marked execution")
struct SecretJoinTests {

    @Test func aNonOriginatingJoinerGetsTheShownOutcomeByPid() async {
        let k = await seeded()
        let own = await send(k, issue(), "alice")
        #expect(isReal(own), "\(own.asJSON.serialize())")
        let joined = await send(k, join(.integer(own.pid!.value)), "eve")
        #expect(isShown(joined), "\(joined.asJSON.serialize())")
        #expect(!joined.asJSON.serialize().contains(KEY))
    }

    @Test func aNonOriginatingJoinerGetsTheShownOutcomeByName() async {
        let k = await seeded()
        let own = await send(k, issue(id: "keyread"), "alice")
        #expect(isReal(own), "\(own.asJSON.serialize())")
        let joined = await send(k, join("keyread"), "eve")
        #expect(isShown(joined), "\(joined.asJSON.serialize())")
        #expect(joined.pid == own.pid)
    }

    @Test func theOriginalCallerJoiningAgainGetsTheRealOutcome() async {
        let k = await seeded()
        let own = await send(k, issue(id: "mine"), "alice")
        let again = await send(k, join("mine"), "alice")
        #expect(isReal(again), "\(again.asJSON.serialize())")
        #expect(again.pid == own.pid)
    }

    /// The operator joining an AGENT's secret execution is not its caller
    /// (shown); an agent joining the OPERATOR's is not either (shown); the
    /// operator joining its own gets the real outcome.
    @Test func theOperatorIsAnIdentityLikeAnyOther() async {
        let k = await seeded()
        let alices = await send(k, issue(id: "a1"), "alice")
        let opJoin = await send(k, join(.integer(alices.pid!.value)))
        #expect(isShown(opJoin), "\(opJoin.asJSON.serialize())")
        let ops = await send(k, issue(id: "o1"))
        #expect(isReal(ops), "\(ops.asJSON.serialize())")
        let eveJoin = await send(k, join("o1"), "eve")
        #expect(isShown(eveJoin), "\(eveJoin.asJSON.serialize())")
        let opAgain = await send(k, join("o1", ["type": "x"]))
        #expect(isReal(opAgain), "\(opAgain.asJSON.serialize())")
    }

    /// A bridge leg relays many callers of ONE peer kernel; each is its
    /// qualified `<guid>|<agent>`, not the leg.
    @Test func twoForeignCallersThroughOneLegAreTwoIdentities() async {
        let k = await seeded()
        let first = await send(k, issue(id: "peer-read"), "leg", remote: "P-g|a")
        #expect(isReal(first), "\(first.asJSON.serialize())")
        let other = await send(k, join("peer-read"), "leg", remote: "P-g|b")
        #expect(isShown(other), "\(other.asJSON.serialize())")
        let same = await send(k, join("peer-read"), "leg", remote: "P-g|a")
        #expect(isReal(same), "\(same.asJSON.serialize())")
    }

    @Test func aJoinerWaitingOnAnExecutionInFlightGetsItShown() async {
        let latch = Latch()
        let k = await seeded(latch)
        let owner = Task { await send(k, issue(id: "slow", verb: "slow_issue"), "alice") }
        for _ in 0..<500 where k.ledger.resolveName("slow") == nil {
            try? await Task.sleep(nanoseconds: 1_000_000)
        }
        #expect(k.ledger.resolveName("slow") != nil)
        let joiner = Task { await send(k, join("slow", ["type": "x"]), "eve") }
        try? await Task.sleep(nanoseconds: 20_000_000)
        latch.release()
        let own = await owner.value
        let joined = await joiner.value
        #expect(isReal(own), "\(own.asJSON.serialize())")
        #expect(isShown(joined), "\(joined.asJSON.serialize())")
    }

    @Test func aJoinOfAnUnmarkedExecutionIsUnchanged() async {
        let k = await seeded()
        let own = await send(
            k, Item("vault", ["type": "echo", "text": "hi"], id: "plain"), "alice")
        let joined = await send(k, join("plain", ["type": "echo"]), "eve")
        #expect(joined.result == own.result && own.result == ["echo": "hi"])
    }

    /// The question ai_core asks before a model's `<exec_id>` joins anything: an
    /// int is a message id, a string a caller's name; anything else, an id naming
    /// no held execution, and an unmarked execution are all False.
    @Test func carriedSecretResolvesAnItemIdAsSendDoes() async {
        let k = await seeded()
        let marked = await send(k, issue(id: "m1"), "alice")
        let plain = await send(
            k, Item("vault", ["type": "echo", "text": "x"], id: "p1"), "alice")
        #expect(k.carriedSecret(.integer(marked.pid!.value)))
        #expect(k.carriedSecret("m1"))
        #expect(!k.carriedSecret(.integer(plain.pid!.value)))
        #expect(!k.carriedSecret("p1"))
        #expect(!k.carriedSecret(.string(String(marked.pid!.value))), "a string is a name")
        for nothing: JSON in [true, .null, 10_000, "nobody", ["m1"]] {
            #expect(!k.carriedSecret(nothing), "\(nothing.serialize())")
        }
    }
}
