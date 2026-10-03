// Two edges of SECRET marks (owner-approved 2026-10-03):
//
// - S2 — a mark on a `create_agent`/`update_agent` payload field is refused: the
//   kernel writes every such field into the agent's RECORD (its `agent_created` /
//   `agent_updated` events, `agent.yaml`), where the mark cannot hold. A value an
//   agent must keep is a `credential_store` ref.
// - S7 — the scrub replaces a dict KEY, a number and a bool whose text equals a
//   secret value: a reply keyed by the token it was given, or a PIN echoed as an int, used
//   to land in every record. Equal, not contained: a key is structure.
//
// Twin of python `tests/kernel/test_secret_record_and_scrub.py`.

import FantasticJSON
import Foundation
import OrderedCollections
import Testing

@testable import FantasticKernel

private let KEY = "sk-record-0123456789abcdef"
private let PIN: Int64 = 987_654_321
private let MOD = "scrub_probe.tools"
private let WHY = "a record field cannot be secret — use a credential_store ref"

private struct Probe: AgentBundle {
    let name = "scrub_probe"
    func handle(agentId: AgentId, payload: JSON, kernel: Kernel) async throws -> JSON? {
        switch payload["type"].asString ?? "" {
        case "boot", "shutdown", "reflect":
            return .object([
                "id": .string(agentId.value), "sentence": "scrub probe", "verbs": .object([:]),
            ])
        case "open_session":
            var sessions = OrderedDictionary<String, JSON>()
            sessions[payload["token"].asString ?? ""] = ["open": true]
            return .object(["sessions": .object(sessions), "count": 1])
        case "echo_pin":
            return .object(["pin": payload["pin"], "tries": 3])
        default:
            return .object(["error": .string("unknown verb \(pyRepr(payload["type"]))")])
        }
    }
}

private func seeded() async -> Kernel {
    let r = BundleRegistry()
    r.register(MOD, Probe())
    let k = Kernel(storage: .inMemory, bundles: r)
    let root = Agent(id: "kernel_state", handlerModule: nil, parentId: nil, meta: ["guid": "g1"])
    k.register(root)
    k.setRoot(root)
    let out = await k.send([
        Item(
            "kernel",
            [
                "type": "create_agent", "parent_id": "kernel_state", "handler_module": .string(MOD),
                "id": "vault",
            ])
    ])[0]
    #expect(out.result["id"] == "vault", "\(out.result.serialize())")
    return k
}

private func marked(_ target: AgentId, _ payload: JSON, _ secret: JSON, id: String? = nil) -> Item {
    var it = Item(target, payload, id: id)
    it.secret = secret
    return it
}

private func ledger(_ k: Kernel) async -> [JSON] {
    let r = await k.send([Item("kernel", ["type": "ledger"])])[0].result
    return r["executions"].asArray ?? []
}

private func refusalText(_ field: LaunchField<Secret?>) -> String? {
    if case .refused(let r) = field { return r["error"].asString }
    return nil
}

@Suite("secret marks: record fields and the scrub")
struct SecretRecordAndScrubTests {

    // ─── S2: a record field is never secret ─────────────────────────────────

    @Test func aMarkOnACreateOrUpdateAgentFieldIsRefused() async {
        let k = await seeded()
        let create: JSON = [
            "type": "create_agent", "parent_id": "kernel_state", "handler_module": .string(MOD),
            "id": "leaky", "env": ["TOKEN": .string(KEY)],
        ]
        let cases: [(JSON, JSON, String)] = [
            (
                create, ["payload.env.TOKEN"],
                "path 'payload.env.TOKEN' names a field create_agent writes into the agent "
                    + "record: \(WHY)"
            ),
            (
                create, [["payload", "env"]],
                "path ['payload', 'env'] names a field create_agent writes into the agent "
                    + "record: \(WHY)"
            ),
            (
                create, ["payload.id"],
                "path 'payload.id' names a field create_agent writes into the agent record: "
                    + WHY
            ),
            (
                ["type": "update_agent", "id": "vault", "note": .string(KEY)], ["payload.note"],
                "path 'payload.note' names a field update_agent writes into the agent record: "
                    + WHY
            ),
        ]
        for (payload, secret, text) in cases {
            let out = await k.send([marked("kernel", payload, secret)])[0]
            #expect(out.status == .failed && out.pid == nil, "\(secret.serialize())")
            #expect(
                out.result
                    == .object([
                        "error": .string("send: secret \(text)"), "reason": "bad_arg",
                        "arg": "secret",
                    ]), "\(out.result.serialize())")
        }
        #expect(k.agent(AgentId("leaky")) == nil)
        #expect(k.agent(AgentId("vault"))?.meta["note"] == nil)
    }

    @Test func payloadTypeIsStillJudgedFirst() {
        let r = Secret.check(
            ["payload.type"], payload: ["type": "update_agent", "id": "x"], blob: nil)
        #expect(
            refusalText(r)
                == "send: secret cannot hide payload.type — the verb is how every row is read")
    }

    /// Only PAYLOAD fields become the record; a result mark masks the call's
    /// records and nothing else, as on any verb.
    @Test func aResultMarkOnCreateAgentIsUnchanged() async {
        let k = await seeded()
        let out = await k.send([
            marked(
                "kernel",
                [
                    "type": "create_agent", "parent_id": "kernel_state",
                    "handler_module": .string(MOD), "id": "plain",
                ], ["result"])
        ])[0]
        #expect(
            out.status == .completed && out.result["id"] == "plain", "\(out.asJSON.serialize())")
    }

    // ─── S7: keys and numbers ───────────────────────────────────────────────

    /// The verb's TEXT is scrubbed as before; the `type` KEY survives.
    @Test func aShortSecretNeverManglesAPayloadKey() {
        let p: JSON = ["type": "set_key", "key": "y"]
        guard case .ok(let s?) = Secret.check(["payload.key"], payload: p, blob: nil) else {
            Issue.record("the mark must pass")
            return
        }
        #expect(s.shownPayload(p, blob: nil).json == ["type": "set_ke<secret>", "key": "<secret>"])
    }

    @Test func scrubReplacesAKeyANumberAndABoolWhoseTextIsASecret() {
        let got = Secret.scrub(
            [
                "a": "xxabcxx", "k": ["abc"], "abc": "ab", "xabcx": 1, "n": 123, "m": 1234,
                "f": 1.5, "t": true, "u": false, "z": nil,
            ], ["ab", "abc", "123", "1.5", "True"])
        let want: JSON = [
            "a": "xx<secret>xx", "k": ["<secret>"], "<secret>": "<secret>",
            // a key that only CONTAINS a secret is structure, kept
            "xabcx": 1,
            "n": "<secret>", "m": 1234, "f": "<secret>",
            // a bool whose text (`True`/`False`) is a secret is scrubbed; null holds none
            "t": "<secret>", "u": false, "z": nil,
        ]
        #expect(got == want, "\(got.serialize())")
    }

    @Test func aReplyKeyedByTheSecretIsRecordedScrubbed() async {
        let k = await seeded()
        let out = await k.send([
            marked(
                "vault", ["type": "open_session", "token": .string(KEY)], ["payload.token"],
                id: "sess-1")
        ])[0]
        #expect(out.result == ["sessions": [KEY: ["open": true]], "count": 1])
        let rows = await ledger(k)
        let row = rows.first { $0["id"] == .integer(out.pid!.value) }
        #expect(
            row?["outcome"]["result"] == ["sessions": ["<secret>": ["open": true]], "count": 1],
            "\(row?.serialize() ?? "no row")")
        #expect(!JSON.array(rows).serialize().contains(KEY))
    }

    @Test func aSecretNumberEchoedAsANumberIsRecordedScrubbed() async {
        let k = await seeded()
        let out = await k.send([
            marked(
                "vault", ["type": "echo_pin", "pin": .integer(PIN)], ["payload.pin"], id: "pin-1")
        ])[0]
        #expect(out.result == ["pin": .integer(PIN), "tries": 3])
        let rows = await ledger(k)
        let row = rows.first { $0["id"] == .integer(out.pid!.value) }
        #expect(
            row?["outcome"]["result"] == ["pin": "<secret>", "tries": 3],
            "\(row?.serialize() ?? "no row")")
        #expect(!JSON.array(rows).serialize().contains(String(PIN)))
    }
}
