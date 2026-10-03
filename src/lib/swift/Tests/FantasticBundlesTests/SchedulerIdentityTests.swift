// A schedule fires AS ITS CREATOR — the scheduler is no deputy.
//
// Audit AV-6 (2026-10-03): every fire went out under the SCHEDULER's identity, so
// anyone allowed to call `schedule`/`tick_now` on it borrowed its `can_call`, and
// `history` handed them the replies. Measured: an agent `b` that could not read
// the vault directly scheduled a vault `read`, ticked it, and read TOP-SECRET out
// of `history`. Now the schedule records WHO scheduled it (`owner`) and fires
// under that identity, so a schedule can do only what its creator could; an
// operator-made schedule fires as the operator. A caller lists, ticks, pauses,
// resumes, unschedules and reads the history of ITS OWN schedules only (the
// operator: all).
//
// Twin of python `scheduler/tests/test_scheduler_identity.py`.

import FantasticJSON
import Foundation
import OrderedCollections
import Testing

@testable import FantasticKernel

private let SECRET = "TOP-SECRET-PAYROLL"
private let SCHED_VERBS: JSON = ["schedule", "tick_now", "history", "list", "unschedule", "pause"]

private func op(_ k: Kernel, _ target: String, _ payload: JSON, as sender: String? = nil) async
    -> Outcome
{
    let item = Item(AgentId(target), payload)
    guard let sender else { return await k.send([item])[0] }
    return await KernelTaskLocals.$currentSender.withValue(AgentId(sender)) {
        await k.send([item])[0]
    }
}

private func create(_ k: Kernel, _ meta: OrderedDictionary<String, JSON>) async {
    var p: OrderedDictionary<String, JSON> = ["type": "create_agent", "parent_id": "kernel_state"]
    for (key, v) in meta { p[key] = v }
    let out = await op(k, "kernel_state", .object(p))
    #expect(out.result["id"] == p["id"], "\(out.result.serialize())")
}

/// Python `k.create(None, id=…, can_call=…)`: a bare caller — a record, no bundle.
private func bare(_ k: Kernel, _ id: String, _ canCall: JSON) {
    k.register(
        Agent(
            id: AgentId(id), handlerModule: nil, parentId: AgentId("kernel_state"),
            meta: ["can_call": canCall]))
}

private func lines(_ r: JSON) -> [String] {
    (r["lines"].asArray ?? []).map { $0["text"].asString ?? "" }
}

/// Python `_wire` on a store-backed kernel: `vault` (a file holding SECRET)
/// answers the SCHEDULER, never `b`; `pad` answers `b` and not the scheduler; `b`
/// may call the scheduler's verbs. Every file lives under a fresh temp dir.
private func wired(_ tag: String) async throws -> (Kernel, URL) {
    let fm = FileManager.default
    let rel = "fantastic-\(tag)-\(UUID().uuidString)"
    let dir = URL(fileURLWithPath: fm.currentDirectoryPath).appendingPathComponent(rel)
    try fm.createDirectory(at: dir, withIntermediateDirectories: true)
    let k = makeKernelWithAll(rootPath: dir)
    await create(k, [
        "handler_module": "file_bridge.tools", "allow": ["."], "id": "store",
        "root": .string(rel), "ingress_rule": "allow_all_ingress", "callable_by": ["sched": "*"],
    ])
    await create(k, [
        "id": "vault", "handler_module": "file_bridge.tools", "root": .string(rel),
        "allow": ["secret.txt"], "ingress_rule": "allow_all_ingress",
        "callable_by": ["sched": "*"],
    ])
    let w = await op(k, "vault", ["type": "write", "path": "secret.txt", "bytes": .string(SECRET)])
    #expect(w.status == .completed, "\(w.asJSON.serialize())")
    await create(k, [
        "id": "pad", "handler_module": "file_bridge.tools", "root": .string(rel),
        "allow": ["note.txt"], "ingress_rule": "allow_all_ingress", "callable_by": ["b": "*"],
    ])
    await create(k, [
        "id": "sched", "handler_module": "scheduler.tools",
        "can_call": ["vault": "*", "store": "*"], "callable_by": ["b": SCHED_VERBS],
    ])
    bare(k, "b", ["sched": SCHED_VERBS, "pad": "*"])
    return (k, dir)
}

private func schedule(_ target: String, _ payload: JSON) -> JSON {
    ["type": "schedule", "target": .string(target), "payload": payload, "interval_seconds": 3600]
}

// ── an owner key that names nobody (audit ARCH2-4) ──────────────────

private let FIRES_ONLY_AS =
    "; a schedule fires only as its recorded owner (null = the operator, a string = an agent)"
private let OWNER_ABSENT = "scheduler: not fired — the schedule's owner key is absent" + FIRES_ONLY_AS
private let OWNER_NOT_A_NAME =
    "scheduler: not fired — the schedule's owner is not null or a string" + FIRES_ONLY_AS

/// One sidecar entry as YAML, targeting `pad` (which answers `b` only — never the
/// scheduler); `owner` is the raw YAML value, or nil for no `owner` key at all.
private func storedYaml(_ id: String, nextRun: String, target: String = "pad", owner: String?)
    -> String
{
    let payload = target == "pad"
        ? "    type: write\n    path: note.txt\n    bytes: unowned\n" : "    type: list\n"
    return "- id: \(id)\n  target: \(target)\n  payload:\n\(payload)"
        + "  interval_seconds: 3600\n  created_at: 0\n  next_run: \(nextRun)\n"
        + "  run_count: 0\n  paused: false\n" + (owner.map { "  owner: \($0)\n" } ?? "")
}

/// Seed `sched`'s sidecar THROUGH the loader, as the operator — before any verb
/// has loaded it into the scheduler's cache.
private func seed(_ k: Kernel, _ yaml: String) async {
    let w = await op(
        k, "kernel_state",
        [
            "type": "persist_blob", "agent_id": "sched", "name": "schedules.yaml",
            "content": .string(yaml),
        ])
    #expect(w.status == .completed && w.result["error"] == .null, "\(w.asJSON.serialize())")
}

@Suite("scheduler — a schedule fires as its creator")
struct SchedulerIdentityTests {

    @Test func aScheduleCannotReachWhatItsCreatorCannot() async throws {
        let (k, dir) = try await wired("sid1")
        defer { try? FileManager.default.removeItem(at: dir) }
        let direct = await op(k, "vault", ["type": "read", "path": "secret.txt"], as: "b")
        #expect(direct.result["reason"] == "unauthorized", "\(direct.asJSON.serialize())")
        let made = await op(
            k, "sched", schedule("vault", ["type": "read", "path": "secret.txt"]), as: "b")
        let sch = made.result["schedule"]
        #expect(sch["owner"] == "b", "\(sch.serialize())")
        let fired = await op(k, "sched", ["type": "tick_now", "schedule_id": sch["id"]], as: "b")
        #expect(fired.result == ["fired": true, "schedule_id": sch["id"]], "\(fired.asJSON.serialize())")
        let hist = await op(k, "sched", ["type": "history"], as: "b")
        #expect(!hist.asJSON.serialize().contains(SECRET), "\(hist.asJSON.serialize())")
        let events = hist.result["history"].asArray ?? []
        #expect(events.count == 1, "\(hist.asJSON.serialize())")
        let event = events.first ?? .null
        #expect(event["owner"] == "b" && event["result"] == .null, "\(event.serialize())")
        #expect(
            (event["error"].asString ?? "").contains("b may not call 'read' on vault"),
            "\(event.serialize())")
    }

    @Test func aScheduleReachesWhatItsCreatorCanAndItsDeputyCannot() async throws {
        let (k, dir) = try await wired("sid2")
        defer { try? FileManager.default.removeItem(at: dir) }
        let made = await op(
            k, "sched",
            schedule("pad", ["type": "write", "path": "note.txt", "bytes": "from-b"]), as: "b")
        let sid = made.result["schedule_id"]
        _ = await op(k, "sched", ["type": "tick_now", "schedule_id": sid], as: "b")
        let read = await op(k, "pad", ["type": "read", "path": "note.txt"])
        #expect(lines(read.result) == ["from-b"], "\(read.asJSON.serialize())")
    }

    /// No grant on either end: the operator's schedule is the operator's call.
    @Test func anOperatorScheduleFiresAsTheOperator() async throws {
        let (k, dir) = try await wired("sid3")
        defer { try? FileManager.default.removeItem(at: dir) }
        let made = await op(
            k, "sched", schedule("pad", ["type": "write", "path": "note.txt", "bytes": "from-op"]))
        let sch = made.result["schedule"]
        #expect(sch["owner"] == .null && sch.asObject?["owner"] != nil, "\(sch.serialize())")
        _ = await op(k, "sched", ["type": "tick_now", "schedule_id": sch["id"]])
        let read = await op(k, "pad", ["type": "read", "path": "note.txt"])
        #expect(lines(read.result) == ["from-op"], "\(read.asJSON.serialize())")
    }

    @Test func aCallerSeesAndDrivesOnlyItsOwnSchedules() async throws {
        let (k, dir) = try await wired("sid4")
        defer { try? FileManager.default.removeItem(at: dir) }
        let mine = await op(k, "sched", schedule("pad", ["type": "list"]), as: "b")
        let theirs = await op(
            k, "sched", schedule("vault", ["type": "read", "path": "secret.txt"]))
        let mineId = mine.result["schedule_id"]
        let opId = theirs.result["schedule_id"]
        _ = await op(k, "sched", ["type": "tick_now", "schedule_id": opId])
        let listed = await op(k, "sched", ["type": "list"], as: "b")
        #expect(
            (listed.result["schedules"].asArray ?? []).map { $0["id"] } == [mineId],
            "\(listed.asJSON.serialize())")
        let everything = await op(k, "sched", ["type": "list"])
        #expect(
            Set((everything.result["schedules"].asArray ?? []).map { $0["id"] }) == [mineId, opId])
        let hist = await op(k, "sched", ["type": "history"], as: "b")
        #expect(
            hist.result["count"] == 0 && !hist.asJSON.serialize().contains(SECRET),
            "\(hist.asJSON.serialize())")
        let gone: [(String, JSON)] = [
            ("tick_now", ["error": .string("schedule \(pyRepr(opId)) not found")]),
            ("pause", ["paused": false, "schedule_id": opId]),
            ("unschedule", ["removed": false, "schedule_id": opId]),
        ]
        for (verb, want) in gone {
            let out = await op(k, "sched", ["type": .string(verb), "schedule_id": opId], as: "b")
            #expect(out.result == want, "\(verb) \(out.asJSON.serialize())")
        }
        let after = await op(k, "sched", ["type": "list"])
        let all = after.result["schedules"].asArray ?? []
        #expect(Set(all.map { $0["id"] }) == [mineId, opId])
        #expect(!all.contains { $0["paused"] == true }, "\(after.asJSON.serialize())")
        let opHist = await op(k, "sched", ["type": "history"])
        #expect(opHist.result["count"] == 1, "\(opHist.asJSON.serialize())")
    }

    /// A call relayed by a bridge leg is the FOREIGN caller's (`<guid>|<agent>`):
    /// its schedule records the caller and the leg (`via`), and fires as that
    /// same pair — the target's `callable_by` judges the foreign id, as on the
    /// wire.
    @Test func aForeignCallersScheduleFiresAsThatCallerViaItsLeg() async throws {
        let (k, dir) = try await wired("sid5")
        defer { try? FileManager.default.removeItem(at: dir) }
        bare(k, "leg", ["sched": SCHED_VERBS, "pad": "*"])
        for (agent, verbs) in [("sched", SCHED_VERBS), ("pad", "*" as JSON)] {
            var cb = k.agent(AgentId(agent))?.metaValue(forKey: "callable_by")?.asObject ?? [:]
            cb["peer-g|x"] = verbs
            k.update(AgentId(agent), ["callable_by": .object(cb)])
        }
        let made = await KernelTaskLocals.$currentSender.withValue(AgentId("leg")) {
            await KernelTaskLocals.withRemoteCaller(leg: AgentId("leg"), address: "peer-g|x") {
                await k.send([
                    Item(
                        "sched",
                        schedule(
                            "pad", ["type": "write", "path": "note.txt", "bytes": "from-peer"]))
                ])[0].result
            }
        }
        let sch = made["schedule"]
        #expect(sch["owner"] == "peer-g|x" && sch["via"] == "leg", "\(made.serialize())")
        _ = await op(k, "sched", ["type": "tick_now", "schedule_id": sch["id"]])
        let read = await op(k, "pad", ["type": "read", "path": "note.txt"])
        #expect(lines(read.result) == ["from-peer"], "\(read.asJSON.serialize())")
    }

    /// An ABSENT `owner` key read as null fired with the operator's ungated
    /// authority (and `asString` read a non-string owner the same way). The key
    /// must be present — null or a string; anything else is refused, nothing is
    /// sent, and the history says why. Twin of python
    /// `test_a_schedule_whose_owner_names_nobody_is_refused_not_fired`.
    @Test func aScheduleWhoseOwnerNamesNobodyIsRefusedNotFired() async throws {
        let (k, dir) = try await wired("sid6")
        defer { try? FileManager.default.removeItem(at: dir) }
        await seed(
            k,
            storedYaml("s_absent", nextRun: "1.0e+12", owner: nil)
                + storedYaml("s_number", nextRun: "1.0e+12", owner: "5"))
        for (sid, why) in [("s_absent", OWNER_ABSENT), ("s_number", OWNER_NOT_A_NAME)] {
            let out = await op(k, "sched", ["type": "tick_now", "schedule_id": .string(sid)])
            #expect(out.result == ["error": .string(why)], "\(sid) \(out.asJSON.serialize())")
        }
        let read = await op(k, "pad", ["type": "read", "path": "note.txt"])
        #expect(!read.asJSON.serialize().contains("unowned"), "\(read.asJSON.serialize())")
        let hist = await op(k, "sched", ["type": "history"])
        let got: [JSON] = (hist.result["history"].asArray ?? []).map {
            [$0["schedule_id"], $0["owner"], $0["result"], $0["error"]]
        }
        #expect(
            got == [
                ["s_absent", .null, .null, .string(OWNER_ABSENT)],
                ["s_number", 5, .null, .string(OWNER_NOT_A_NAME)],
            ], "\(hist.asJSON.serialize())")
    }

    /// The tick path: the ownerless schedule is refused and recorded, the
    /// operator's (`owner: null`, PRESENT) fires as before — in the same sweep.
    @Test func theTickLoopRefusesAnOwnerlessScheduleAndFiresTheRest() async throws {
        let (k, dir) = try await wired("sid7")
        defer { try? FileManager.default.removeItem(at: dir) }
        await seed(
            k,
            storedYaml("s_absent", nextRun: "0", owner: nil)
                + storedYaml("s_op", nextRun: "0", target: "vault", owner: "null"))
        var events: [JSON] = []
        for _ in 0..<100 {
            try await Task.sleep(nanoseconds: 50_000_000)
            events = (await op(k, "sched", ["type": "history"])).result["history"].asArray ?? []
            if events.count >= 2 { break }
        }
        let byId = Dictionary(events.map { ($0["schedule_id"].asString ?? "", $0) }) { a, _ in a }
        #expect(byId["s_absent"]?["error"] == .string(OWNER_ABSENT), "\(events)")
        #expect(byId["s_op"]?["error"] == .null, "\(events)")
        let read = await op(k, "pad", ["type": "read", "path": "note.txt"])
        #expect(!read.asJSON.serialize().contains("unowned"), "\(read.asJSON.serialize())")
    }
}
