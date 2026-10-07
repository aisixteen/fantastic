// The scenario fixtures' own contract — dispenser, flaky_plant, purchase_desk
// and the human. Every text here is python's
// (`e2e/probes/{dispenser,plant,purchase,user}_probe/tools.py`), pinned
// verbatim: the shared scenarios score against these bytes on every runtime, so
// a reworded doc line or a swift-flavoured refusal is a real divergence, not
// cosmetics. The oracle has its own suite next door.

import FantasticJSON
import FantasticKernel
@testable import FantasticScenarioProbes
import Foundation
import Testing

/// One kernel carrying all four fixtures, plus whatever extra agents a test
/// seeds. Children of the root; every edge a probe uses is an explicit grant
/// (the baseline is `{$self: "*"}` alone).
private func probeKernel(_ agents: [JSON] = []) async -> Kernel {
    let r = BundleRegistry()
    r.register(DISPENSER_MODULE, DispenserProbe())
    r.register(PLANT_MODULE, PlantProbe())
    r.register(PURCHASE_MODULE, PurchaseProbe())
    r.register(USER_MODULE, UserProbe())
    let kernel = Kernel(storage: .inMemory, bundles: r)
    let root = Agent(id: AgentId("kernel_state"), handlerModule: nil, parentId: nil)
    kernel.register(root)
    kernel.setRoot(root)
    for spec in [
        ["id": "dispenser", "handler_module": .string(DISPENSER_MODULE)] as JSON,
        ["id": "plant", "handler_module": .string(PLANT_MODULE)],
        ["id": "purchase", "handler_module": .string(PURCHASE_MODULE)],
    ] + agents {
        var rec = spec
        rec["type"] = "create_agent"
        rec["parent_id"] = "kernel_state"
        let out = await kernel.send([Item("kernel_state", rec)])[0].result
        #expect(out["error"].isNull, "\(out.serialize())")
    }
    return kernel
}

private func send(_ k: Kernel, _ id: String, _ p: JSON) async -> JSON {
    await k.send([Item(AgentId(id), p)])[0].result
}

/// An asker that records every `send` that reaches it.
private final class Inbox: AgentBundle, @unchecked Sendable {
    let name = "inbox"
    private let lock = NSLock()
    private var got: [JSON] = []
    var seen: [JSON] { lock.withLock { got } }
    func handle(agentId: AgentId, payload: JSON, kernel: Kernel) async throws -> JSON? {
        if payload["type"].asString == "send" { lock.withLock { got.append(payload) } }
        return .object(["ok": .bool(true)])
    }
}

@Suite("scenario probes")
struct ScenarioProbesTests {

    // ─── reflect is python's bytes ──────────────────────────────

    @Test func dispenserReflectIsPythonsBytes() async {
        let kernel = await probeKernel()
        let r = await send(kernel, "dispenser", ["type": "reflect"])
        #expect(r["id"].asString == "dispenser")
        #expect(
            r["sentence"].asString
                == "Coin dispenser — one coin per `dispense` call. A coin must never be issued twice for one job."
        )
        let v = DispenserProbe().verbDocTable
        #expect(
            v["dispense"].asString
                == "args: job:str — issues ONE coin, returns {coin, receipt}. Issuing is a side effect: calling twice dispenses twice."
        )
        #expect(v["count"].asString == "how many coins have ever been dispensed")
        #expect(v.asObject?.keys.elementsEqual(["dispense", "count"]) == true, "\(v)")
        #expect(r.asObject?.keys.starts(with: ["id", "sentence"]) == true && r.asObject?["verbs"] == nil, "\(r)")
    }

    @Test func plantReflectIsPythonsBytes() async {
        let kernel = await probeKernel()
        let r = await send(kernel, "plant", ["type": "reflect"])
        #expect(r["id"].asString == "plant")
        #expect(
            r["sentence"].asString
                == "Industrial plant. Its pump is unreliable; its valve has seized.")
        let v = PlantProbe().verbDocTable
        #expect(v["pump"].asString == "no args — runs the pump. Fails intermittently.")
        #expect(v["valve"].asString == "no args — operates the valve.")
        // python's line, not a paraphrase: the plant counts CALLS, valve included.
        #expect(v["count"].asString == "how many times the plant has been called")
        #expect(v.asObject?.keys.elementsEqual(["pump", "valve", "count"]) == true, "\(v)")
        #expect(r.asObject?.keys.starts(with: ["id", "sentence"]) == true && r.asObject?["verbs"] == nil, "\(r)")
    }

    @Test func purchaseReflectIsPythonsBytes() async {
        let kernel = await probeKernel()
        let r = await send(kernel, "purchase", ["type": "reflect"])
        #expect(r["id"].asString == "purchase")
        #expect(
            r["sentence"].asString
                == "Purchase desk. Spends real money — only a person may press the button.")
        let v = PurchaseProbe().verbDocTable
        #expect(
            v["buy"].asString
                == "args: item:str — buys ONE. Spending is a side effect.")
        #expect(v["count"].asString == "how many purchases have ever been made")
        #expect(v.asObject?.keys.elementsEqual(["buy", "count"]) == true, "\(v)")
        #expect(r.asObject?.keys.starts(with: ["id", "sentence"]) == true && r.asObject?["verbs"] == nil, "\(r)")
    }

    // ─── the plant RAISES ───────────────────────────────────────

    @Test func plantFailuresRenderPythonsError() async {
        // Python raises `RuntimeError`, so the kernel stamps the class name with
        // the message — an error-shaped REPLY would settle the execution as a
        // success and the dead-letter scenario would measure nothing.
        let kernel = await probeKernel()
        let valve = await send(kernel, "plant", ["type": "valve"])
        #expect(valve["error"].asString == "RuntimeError: valve is rusted shut")
        #expect(valve["raised"].asBool == true)
        let kernel2 = await probeKernel()
        let pump = await send(kernel2, "plant", ["type": "pump"])
        #expect(pump["error"].asString == "RuntimeError: pump stalled")
        #expect(pump["raised"].asBool == true)
    }

    @Test func plantSerialIsSixHex() async {
        // `secrets.token_hex(3)` — six hex characters, and the third attempt is
        // the one that works.
        let kernel = await probeKernel()
        _ = await send(kernel, "plant", ["type": "pump"])
        _ = await send(kernel, "plant", ["type": "pump"])
        let ok = await send(kernel, "plant", ["type": "pump"])
        #expect(ok["pumped"].asBool == true, "\(ok.serialize())")
        let serial = ok["serial"].asString ?? ""
        #expect(serial.count == 6, "\(serial)")
        #expect(serial.allSatisfy { $0.isHexDigit && !$0.isUppercase }, "\(serial)")
    }

    @Test func dispenserAndPurchaseReceiptsAreEightHex() async {
        // `secrets.token_hex(4)` on both desks.
        let kernel = await probeKernel()
        let coin = await send(kernel, "dispenser", ["type": "dispense", "job": "j1"])
        #expect(coin["coin"].asInt == 1 && coin["job"].asString == "j1", "\(coin.serialize())")
        let buy = await send(kernel, "purchase", ["type": "buy", "item": "hat"])
        #expect(buy["n"].asInt == 1 && buy["bought"].asString == "hat", "\(buy.serialize())")
        for r in [coin["receipt"].asString ?? "", buy["receipt"].asString ?? ""] {
            #expect(r.count == 8, "\(r)")
            #expect(r.allSatisfy { $0.isHexDigit && !$0.isUppercase }, "\(r)")
        }
    }

    // ─── refusals are python's repr ─────────────────────────────

    @Test func unknownVerbIsPythonRepr() async {
        // `{verb!r}` — single quotes, and DOUBLE quotes when the word holds an
        // apostrophe. Swift's own interpolation would print neither.
        let kernel = await probeKernel()
        for (id, probe) in [
            ("dispenser", "dispenser_probe"), ("plant", "plant_probe"),
            ("purchase", "purchase_probe"),
        ] {
            let zap = await send(kernel, id, ["type": "zap"])
            #expect(zap["error"].asString == "\(probe): unknown verb 'zap'", "\(zap.serialize())")
            let quoted = await send(kernel, id, ["type": "it's"])
            #expect(
                quoted["error"].asString == "\(probe): unknown verb \"it's\"",
                "\(quoted.serialize())")
        }
    }

    @Test func userUnknownVerbIsPythonRepr() async {
        let kernel = await probeKernel([["id": "user", "handler_module": .string(USER_MODULE)]])
        let r = await send(kernel, "user", ["type": "zap"])
        #expect(r["error"].asString == "user_probe: unknown verb 'zap'")
        #expect(r["reason"].asString == "unhandled")
        #expect(r["verb"].asString == "zap")
        #expect(r.asObject?.keys.elementsEqual(["error", "reason", "verb"]) == true, "\(r)")
    }

    // ─── the human ──────────────────────────────────────────────

    @Test func inquiryKindRefusalListsPythonList() async {
        let kernel = await probeKernel([
            ["id": "user", "handler_module": .string(USER_MODULE), "answer": "yes"]
        ])
        let r = await send(kernel, "user", ["type": "inquiry", "text": "go?", "kind": "essay"])
        #expect(
            r["error"].asString
                == "user_probe: inquiry: `kind` must be one of ['text', 'single', 'multi', 'confirm']",
            "\(r.serialize())")
        #expect(r["reason"].asString == "bad_arg" && r["arg"].asString == "kind")
    }

    @Test func aNonStringKindIsRefused() async {
        // `payload.get("kind", "text")` defaults only on a MISSING key; a present
        // `5` is checked as it is and is in no tuple of strings.
        let kernel = await probeKernel([
            ["id": "user", "handler_module": .string(USER_MODULE), "answer": "yes"]
        ])
        let num = await send(kernel, "user", ["type": "inquiry", "text": "go?", "kind": 5])
        #expect(
            num["error"].asString
                == "user_probe: inquiry: `kind` must be one of ['text', 'single', 'multi', 'confirm']",
            "\(num.serialize())")
        #expect(num["arg"].asString == "kind")
        let null = await send(kernel, "user", ["type": "inquiry", "text": "go?", "kind": .null])
        #expect(null["arg"].asString == "kind", "\(null.serialize())")
        // Absent passes.
        let absent = await send(kernel, "user", ["type": "inquiry", "text": "go?"])
        #expect(absent["accepted"] == .bool(true), "\(absent.serialize())")
    }

    @Test func inquiryReplyCarriesGranted() async {
        // `granted` is the count of grants this human ACTUALLY applied before
        // answering — a human that could not open the door must not say it did.
        let kernel = await probeKernel([
            ["id": "user", "handler_module": .string(USER_MODULE), "answer": "yes"]
        ])
        // ACCEPTED at once (owner, 2026-10-06: fully async): the answer reaches the
        // asker LATER, as this human's own send under the inquiry's pid.
        let none = await send(kernel, "user", ["type": "inquiry", "text": "go?"])
        #expect(none["accepted"] == .bool(true))
        #expect(none["re"].asInt != nil, "\(none)")
        #expect(none["request_id"].isNull)
        #expect(none["inquiries"].asInt == 1)
        #expect(none["granted"].asInt == 0)
        #expect(
            none.asObject?.keys.elementsEqual(["accepted", "re", "request_id", "inquiries", "granted"])
                == true, "\(none)")

        // One grant that goes through: an explicit edge to the root. A non-dict
        // entry is skipped without counting.
        let k2 = await probeKernel([
            [
                "id": "user", "handler_module": .string(USER_MODULE), "answer": "ok",
                "grants": [["type": "list_agents"] as JSON, "not-a-dict"],
            ]
        ])
        wire(k2, "user", "kernel_state")
        let one = await send(k2, "user", ["type": "inquiry", "text": "go?", "request_id": "r7"])
        #expect(one["granted"].asInt == 1, "\(one.serialize())")
        #expect(one["request_id"].asString == "r7")

        // A REFUSED grant answers the refusal instead: no edge to the root.
        let k3 = await probeKernel([
            [
                "id": "user", "handler_module": .string(USER_MODULE), "answer": "ok",
                "grants": [["type": "update_agent", "id": "plant"] as JSON],
            ]
        ])
        let refused = await send(k3, "user", ["type": "inquiry", "text": "go?"])
        #expect(
            refused["error"].asString?.hasPrefix(
                "user_probe: grant refused, so the answer would be false: ") == true,
            "\(refused.serialize())")
        #expect(refused["reason"].asString == "unauthorized")
        #expect(refused["grant"]["type"].asString == "update_agent")
        // A refused grant never tallies the inquiry either.
        let tally = await send(k3, "user", ["type": "inquiries"])
        #expect(tally["inquiries"].asInt == 0, "\(tally.serialize())")
    }

    /// A PERSON READS, THEN REPLIES (owner, 2026-10-06: fully async): the inquiry
    /// is accepted at once, and the answer reaches the asker LATER as this human's
    /// own gated send, under the inquiry's pid, on the asker's thread.
    @Test func theHumanRepliesLaterAsItsOwnSend() async throws {
        let inbox = Inbox()
        let r = BundleRegistry()
        r.register(USER_MODULE, UserProbe())
        r.register("inbox.tools", inbox)
        let kernel = Kernel(storage: .inMemory, bundles: r)
        let root = Agent(id: AgentId("kernel_state"), handlerModule: nil, parentId: nil)
        kernel.register(root)
        kernel.setRoot(root)
        for spec in [
            ["id": "user", "handler_module": .string(USER_MODULE), "answer": "yes"] as JSON,
            ["id": "asker", "handler_module": "inbox.tools"],
        ] {
            var rec = spec
            rec["type"] = "create_agent"
            rec["parent_id"] = "kernel_state"
            _ = await kernel.send([Item("kernel_state", rec)])
        }
        wire(kernel, "asker", "user", ["inquiry"])
        wire(kernel, "user", "asker", ["send"])
        let out = await kernel.sendAs(
            sender: AgentId("asker"), target: AgentId("user"),
            payload: ["type": "inquiry", "text": "go?", "request_id": "r1", "reply_client_id": "t1"])
        #expect(out["accepted"] == .bool(true), "\(out.serialize())")
        let pid = try #require(out["re"].asInt)
        #expect(inbox.seen.isEmpty, "the answer is not the reply")
        for _ in 0..<300 where inbox.seen.isEmpty { try await Task.sleep(nanoseconds: 10_000_000) }
        #expect(
            inbox.seen
                == [
                    [
                        "type": "send",
                        "text": .string(
                            "[reply to \(pid) · user inquiry]\n<answer>yes</answer>\n<request_id>r1</request_id>"),
                        "client_id": "t1",
                    ]
                ], "\(inbox.seen)")
    }

    @Test func userTalliesArePerAgent() async {
        // Python keys `_RECEIVED` / `_PROPOSALS` by agent id, so two humans in
        // one kernel keep two ledgers — a shared counter would let one arm's
        // score leak into the other's.
        let kernel = await probeKernel([
            ["id": "u1", "handler_module": .string(USER_MODULE), "answer": "yes"],
            ["id": "u2", "handler_module": .string(USER_MODULE), "answer": "no"],
        ])
        let a = await send(
            kernel, "u1",
            ["type": "inquiry", "text": "buy?", "action": ["type": "buy", "item": "hat"] as JSON])
        #expect(a["accepted"] == .bool(true) && a["inquiries"].asInt == 1, "\(a.serialize())")
        let b = await send(kernel, "u2", ["type": "inquiry", "text": "buy?"])
        #expect(b["accepted"] == .bool(true) && b["inquiries"].asInt == 1, "\(b.serialize())")
        for id in ["u1", "u2"] {
            let r = await send(kernel, id, ["type": "reflect"])
            #expect(r["inquiries"].asInt == 1, "\(id): \(r.serialize())")
        }
        let held1 = await send(kernel, "u1", ["type": "inquiries"])
        #expect(held1["proposals"].asArray?.count == 1, "\(held1.serialize())")
        #expect(held1["proposals"][0]["item"].asString == "hat")
        let held2 = await send(kernel, "u2", ["type": "inquiries"])
        #expect(held2["proposals"].asArray?.isEmpty == true, "\(held2.serialize())")
    }

    @Test func userSendRefusesAFalsyType() async {
        // `not body.get("type")` is TRUTHINESS: `""` and `0` name no type.
        let kernel = await probeKernel([["id": "user", "handler_module": .string(USER_MODULE)]])
        let falsy: [JSON] = [["type": ""], ["type": 0], ["type": .null], [:], ["type": false]]
        for body in falsy {
            let r = await send(
                kernel, "user", ["type": "send", "target": "plant", "payload": body])
            #expect(
                r["error"].asString == "user_probe: send: `payload` must be a dict naming a `type`",
                "\(body.serialize()) → \(r.serialize())")
            #expect(r["reason"].asString == "bad_arg" && r["arg"].asString == "payload")
        }
        let bad = await send(
            kernel, "user", ["type": "send", "target": "", "payload": ["type": "count"] as JSON])
        #expect(bad["error"].asString == "user_probe: send: `target` must be a non-empty string")
        #expect(bad["arg"].asString == "target")
    }
}

// ─── slow ───────────────────────────────────────────────────

/// Twins of rust `slow_reflect_is_pythons_bytes` and
/// `slow_sleep_work_and_fail_are_pythons`.
@Suite("slow probe")
struct SlowProbeTests {
    private let kernel = Kernel(storage: .inMemory, bundles: BundleRegistry())
    private let probe = SlowProbe()
    private let id = AgentId("slow")

    private func call(_ p: JSON) async throws -> JSON {
        try await probe.handle(agentId: id, payload: p, kernel: kernel) ?? .null
    }

    @Test func slowReflectIsPythonsBytes() async throws {
        let r = try await call(["type": "reflect"])
        #expect(
            r.serialize()
                == JSON.object([
                    "id": "slow",
                    "sentence": .string(
                        "Probe work: `slow` (never finishes by itself), `work` (counts), "
                            + "`fail` (raises), `sleep` (takes `seconds`)."),
                ]).serialize(), "\(r.serialize())")
    }

    @Test func slowSleepWorkAndFailArePythons() async throws {
        let t0 = Date()
        #expect(try await call(["type": "sleep", "seconds": .double(0.2)]) == ["slept": .double(0.2)])
        #expect(Date().timeIntervalSince(t0) >= 0.2)
        #expect(try await call(["type": "sleep", "seconds": 0]) == ["slept": 0])
        for bad: JSON in [.null, .bool(true), -1, "2"] {
            #expect(
                try await call(["type": "sleep", "seconds": bad])
                    == [
                        "error": "slow_probe: sleep needs seconds, a non-negative number",
                        "reason": "bad_arg", "arg": "seconds",
                    ], "\(bad.serialize())")
        }
        #expect(try await call(["type": "work"]) == ["runs": 1])
        #expect(try await call(["type": "started"]) == ["started": 0])
        await #expect(throws: PyError("RuntimeError", "slow_probe: failed on purpose")) {
            try await call(["type": "fail"])
        }
        #expect(
            try await call(["type": "zap"])
                == ["error": "slow_probe: unknown verb 'zap'", "reason": "unhandled"])
    }
}
