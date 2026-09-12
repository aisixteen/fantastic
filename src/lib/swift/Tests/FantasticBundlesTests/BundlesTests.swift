// Smoke tests for every Phase 3 bundle.
//
// Each bundle gets a smoke test covering reflect + the most
// important verb. Detailed bundle-specific behavior tests live in
// per-bundle test targets in subsequent commits; this target proves
// every bundle compiles, registers, and answers its primary verb.

import FantasticCliBundle
import FantasticFile
import FantasticJSON
import FantasticKernel
import FantasticKernelBridge
import FantasticKernelStartup
import FantasticProxyAgent
import FantasticScheduler
import FantasticTools
import FantasticWeb
import FantasticWebRest
import FantasticWebWS
import Foundation
import Testing

func makeKernelWithAll(rootPath: URL = URL(fileURLWithPath: "")) -> Kernel {
    let registry = BundleRegistry()
    registry.register("file_bridge.tools", FileBundle())
    registry.register("proxy_agent.tools", ProxyAgentBundle())
    registry.register("tools.tools", ToolsBundle())
    registry.register("scheduler.tools", SchedulerBundle())
    registry.register("ws_bridge.tools", KernelBridgeBundle())
    registry.register("web_ws.tools", WebWSBundle())
    registry.register("web_rest.tools", WebRestBundle())
    let kernel = Kernel(storage: .inMemory, bundles: registry)
    // The root's own dir IS the store root: `findStore` discovers a
    // `file_bridge` child whose `root` resolves to it, which is how the loader
    // owns sidecar placement.
    let root = Agent(id: "core", handlerModule: nil, parentId: nil, rootPath: rootPath)
    kernel.register(root)
    kernel.setRoot(root)
    return kernel
}

/// A disk-mode kernel rooted at `workdir/.fantastic`, with the full registry —
/// for the persistence-inversion test (records persist THROUGH a file_bridge).
func makeDiskKernel(workdir: URL) -> Kernel {
    let registry = BundleRegistry()
    registry.register("file_bridge.tools", FileBundle())
    registry.register("tools.tools", ToolsBundle())
    let kernel = Kernel(storage: .disk(workdir), bundles: registry)
    let root = Agent(
        id: "core", handlerModule: nil, parentId: nil,
        rootPath: workdir.appendingPathComponent(".fantastic"))
    kernel.register(root)
    kernel.setRoot(root)
    return kernel
}

@Suite("Phase 3 bundle smoke tests")
struct BundleSmokeTests {
    @Test func persistInversionThroughDiscoveredStore() async throws {
        // The keystone: records persist THROUGH a discovered file_bridge
        // provider (py/rust parity), NOT a direct substrate write. No provider ⇒
        // RAM (no fallback); a wired store self-persists + carries the rest.
        let fm = FileManager.default
        let workdir = fm.temporaryDirectory.appendingPathComponent(
            "fantastic-inv-\(UUID().uuidString)")
        try fm.createDirectory(
            at: workdir.appendingPathComponent(".fantastic"), withIntermediateDirectories: true)
        defer { try? fm.removeItem(at: workdir) }
        let kernel = makeDiskKernel(workdir: workdir)
        func file(_ rel: String) -> String {
            workdir.appendingPathComponent(".fantastic/\(rel)").path
        }

        // No store wired ⇒ the create stays in RAM (NO direct-fs fallback).
        _ = await kernel.send([Item("core", ["type": "create_agent", "parent_id": "core", "handler_module": "tools.tools", "id": "ram_only"])])[0].result
        #expect(!fm.fileExists(atPath: file("agents/ram_only/agent.json")))
        #expect(kernel.agent("ram_only") != nil)  // live in RAM

        // Wire the store (file_bridge @ .fantastic, open) — it self-persists.
        _ = await kernel.send([Item("core", [
                "type": "create_agent", "parent_id": "core", "handler_module": "file_bridge.tools", "id": "store",
                "root": ".fantastic", "ingress_rule": "allow_all_ingress",
            ])])[0].result
        #expect(
            fm.fileExists(atPath: file("agents/store/agent.json")),
            "the store must persist itself through itself")

        // Now a created agent persists THROUGH the provider.
        _ = await kernel.send([Item("core", [
                "type": "create_agent", "parent_id": "core", "handler_module": "tools.tools", "id": "foo",
                "display_name": "Foo",
            ])])[0].result
        #expect(fm.fileExists(atPath: file("agents/foo/agent.json")))
        let content = try String(contentsOf: URL(fileURLWithPath: file("agents/foo/agent.json")))
        #expect(content.contains("\"foo\"") && content.contains("Foo"))

        // Delete removes the record THROUGH the provider's recursive delete.
        _ = await kernel.send([Item("core", ["type": "delete_agent", "id": "foo"])])[0].result
        #expect(!fm.fileExists(atPath: file("agents/foo")))
    }

    @Test func fileBridgeStreamsRoundTripRawBytes() async throws {
        // The "send+stream handle" — read_stream/write_stream carry RAW BYTES
        // over the symmetric binary channel (never base64), same as py/rust.
        let kernel = makeKernelWithAll()
        let fm = FileManager.default
        let rel = "fantastic-stream-\(UUID().uuidString)"
        let dir = URL(fileURLWithPath: fm.currentDirectoryPath).appendingPathComponent(rel)
        try fm.createDirectory(at: dir, withIntermediateDirectories: true)
        defer { try? fm.removeItem(at: dir) }
        _ = await kernel.send([Item("core", [
                "type": "create_agent", "parent_id": "core", "handler_module": "file_bridge.tools", "id": "fs",
                "root": .string(rel), "ingress_rule": "allow_all_ingress",
            ])])[0].result
        // Non-UTF-8 bytes — proves the channel is raw, not text/base64.
        let payload = Data([0x00, 0xFF, 0xCA, 0xFE, 0xBA, 0xBE, 0x10, 0x80])
        // SINK.
        let (w, _) = await kernel.sendWithBinary(
            "fs",
            .object([
                "type": .string("write_stream"), "path": .string("blob.bin"),
                "truncate": .bool(true),
            ]), payload)
        #expect(w["written"].asInt == Int64(payload.count), "\(w)")
        // SOURCE.
        let (m, body) = await kernel.sendWithBinary(
            "fs",
            .object(["type": .string("read_stream"), "path": .string("blob.bin")]), Data())
        #expect(body == payload, "bytes must round-trip verbatim")
        #expect(m["eof"].asBool == true)
        #expect(m["size"].asInt == Int64(payload.count))
    }

    @Test func fileBundleReflect() async throws {
        let kernel = makeKernelWithAll()
        let tmp = FileManager.default.temporaryDirectory.appendingPathComponent(
            "fantastic-file-test-\(UUID().uuidString)")
        try? FileManager.default.createDirectory(at: tmp, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: tmp) }
        _ = await kernel.send([Item("core", [
                "type": "create_agent",
                "parent_id": "core",
                "handler_module": "file_bridge.tools",
                "id": "fs1",
                "root": .string(tmp.path),
            ])])[0].result
        let r = await kernel.send([Item("fs1", ["type": "reflect"])])[0].result
        #expect(r["kind"].asString == nil || r["sentence"].asString?.contains("Filesystem") == true)
    }

    @Test func proxyAgentNoHostReturnsStructuredError() async {
        let kernel = makeKernelWithAll()
        _ = await kernel.send([Item("core", ["type": "create_agent", "parent_id": "core", "handler_module": "proxy_agent.tools", "id": "p1"])])[0].result
        let r = await kernel.send([Item("p1", ["type": "render", "x": 1])])[0].result
        #expect(r["reason"].asString == "no_host")
    }

    @Test func proxyAgentReflectShowsHostRegisteredFalse() async {
        let kernel = makeKernelWithAll()
        _ = await kernel.send([Item("core", ["type": "create_agent", "parent_id": "core", "handler_module": "proxy_agent.tools", "id": "p2"])])[0].result
        let r = await kernel.send([Item("p2", ["type": "reflect"])])[0].result
        #expect(r["host_registered"].asBool == false)
        #expect(r["kind"].asString == "proxy_agent")
    }

    @Test func toolsRegisterAndDispatch() async {
        _ = FantasticTools.clear()
        let kernel = makeKernelWithAll()
        _ = await kernel.send([Item("core", ["type": "create_agent", "parent_id": "core", "handler_module": "tools.tools", "id": "tools"])])[0].result
        let r = await kernel.send([Item("tools", [
                "type": "register",
                "name": "ping",
                "agent_id": "health",
                "description": "Returns pong.",
                "parameters_schema": ["type": "object"] as JSON,
            ])])[0].result
        #expect(r["ok"].asBool == true)
        let list = await kernel.send([Item("tools", ["type": "list_for_llm"])])[0].result
        let arr = list["tools"].asArray ?? []
        #expect(arr.count == 1)
        #expect(arr[0]["name"].asString == "ping")
    }

    @Test func toolsDispatchUnknownReturnsToolNotFound() async {
        _ = FantasticTools.clear()
        let kernel = makeKernelWithAll()
        _ = await kernel.send([Item("core", ["type": "create_agent", "parent_id": "core", "handler_module": "tools.tools", "id": "tools"])])[0].result
        let r = await kernel.send([Item("tools", ["type": "dispatch", "name": "nope", "arguments": [:] as JSON])])[0].result
        #expect(r["reason"].asString == "tool_not_found")
    }

    @Test func schedulerScheduleListUnschedule() async throws {
        // py/rust contract: schedule {target, payload, interval_seconds} → mints a
        // schedule_id, persisted THROUGH the loader (store-relative
        // agents/<id>/schedules.json); a WRITE failfasts until a store is wired.
        let fm = FileManager.default
        let rel = "fantastic-sched-\(UUID().uuidString)"
        let dir = URL(fileURLWithPath: fm.currentDirectoryPath).appendingPathComponent(rel)
        try fm.createDirectory(at: dir, withIntermediateDirectories: true)
        let kernel = makeKernelWithAll(rootPath: dir)
        defer { try? fm.removeItem(at: dir) }
        // Open file_bridge store — the scheduler persists through it.
        _ = await kernel.send([Item("core", [
                "type": "create_agent", "parent_id": "core", "handler_module": "file_bridge.tools", "id": "store",
                "root": .string(rel), "ingress_rule": "allow_all_ingress",
                // Two-sided deny-all: accept the scheduler that persists through it.
                "callable_by": ["sched": "*"],
            ])])[0].result
        _ = await kernel.send([Item("core", [
                "type": "create_agent", "parent_id": "core", "handler_module": "scheduler.tools", "id": "sched",
                "can_call": ["store": "*"],
            ])])[0].result
        let r = await kernel.send([Item("sched", [
                "type": "schedule", "target": "core", "interval_seconds": .integer(60),
                "payload": ["type": "list_agents"] as JSON,
            ])])[0].result
        let sid = r["schedule_id"].asString
        #expect(sid != nil, "\(r)")
        // store-relative sidecar landed under the store root.
        #expect(
            fm.fileExists(atPath: dir.appendingPathComponent("agents/sched/schedules.json").path))
        let list = await kernel.send([Item("sched", ["type": "list"])])[0].result
        #expect((list["schedules"].asArray ?? []).count == 1)
        // The sidecar landed where the LOADER put it — this bundle named only a
        // filename, never a path.
        // unschedule removes it.
        let u = await kernel.send([Item("sched", ["type": "unschedule", "schedule_id": .string(sid ?? "")])])[0].result
        #expect(u["removed"].asBool == true)
        let after = await kernel.send([Item("sched", ["type": "list"])])[0].result
        #expect((after["schedules"].asArray ?? []).isEmpty)
    }

    @Test func schedulerTickNowFiresAndHistory() async throws {
        // The FIRE path: tick_now dispatches the schedule's payload to its
        // target, bumps run_count, appends a `schedule_fired` event to the
        // history.jsonl sidecar (through the provider), and `history` reads it
        // back. Mirrors the py/rust contract.
        let fm = FileManager.default
        let rel = "fantastic-schedfire-\(UUID().uuidString)"
        let dir = URL(fileURLWithPath: fm.currentDirectoryPath).appendingPathComponent(rel)
        try fm.createDirectory(at: dir, withIntermediateDirectories: true)
        let kernel = makeKernelWithAll(rootPath: dir)
        defer { try? fm.removeItem(at: dir) }
        _ = await kernel.send([Item("core", [
                "type": "create_agent", "parent_id": "core", "handler_module": "file_bridge.tools", "id": "store",
                "root": .string(rel), "ingress_rule": "allow_all_ingress",
                // Two-sided deny-all: accept the scheduler that persists through it.
                "callable_by": ["sched": "*"],
            ])])[0].result
        _ = await kernel.send([Item("core", [
                "type": "create_agent", "parent_id": "core", "handler_module": "scheduler.tools", "id": "sched",
                "can_call": ["store": "*"],
            ])])[0].result
        let r = await kernel.send([Item("sched", [
                "type": "schedule", "target": "core", "interval_seconds": .integer(3600),
                "payload": ["type": "list_agents"] as JSON,
            ])])[0].result
        let sid = try #require(r["schedule_id"].asString)
        // Fire it NOW (no tick-loop wait).
        let fired = await kernel.send([Item("sched", ["type": "tick_now", "schedule_id": .string(sid)])])[0].result
        #expect(fired["fired"].asBool == true, "\(fired)")
        // run_count bumped + persisted.
        let list = await kernel.send([Item("sched", ["type": "list"])])[0].result
        #expect(list["schedules"][0]["run_count"].asInt == 1, "\(list)")
        // history returns the schedule_fired event with the dispatch RESULT
        // (list_agents reply) — proving the target was actually called.
        let h = await kernel.send([Item("sched", ["type": "history", "schedule_id": .string(sid)])])[0].result
        #expect((h["count"].asInt ?? 0) >= 1, "\(h)")
        let ev = h["history"][0]
        #expect(ev["type"].asString == "schedule_fired")
        #expect(ev["scheduler_id"].asString == "sched")
        #expect(ev["error"].isNull, "\(ev)")
        #expect(!ev["result"]["agents"].isNull, "fire must carry the target's reply: \(ev)")
        // The history sidecar landed store-relative, next to schedules.json.
        #expect(
            fm.fileExists(atPath: dir.appendingPathComponent("agents/sched/history.jsonl").path))
    }

    // ── #82 / #145: boot needs no store; one concurrent batch per tick,
    //    persist-before-fire, per-schedule timeout ──────────────────

    /// A store-backed kernel with `probes` probe targets and a scheduler
    /// (tick 0.1s) granted to call them — both sides of the deny-all gate open.
    private func makeProbedScheduler(
        dir: URL, rel: String, probes: [String], capture: URL? = nil
    ) async -> (Kernel, SchedProbe) {
        let kernel = makeKernelWithAll(rootPath: dir)
        let probe = SchedProbe(capture: capture)
        kernel.bundles.register("sched_probe.tools", probe)
        _ = await kernel.send([Item("core", [
                "type": "create_agent", "parent_id": "core", "handler_module": "file_bridge.tools", "id": "store",
                "root": .string(rel), "ingress_rule": "allow_all_ingress",
            ])])[0].result
        var canCall: JSON = .object([:])
        for p in probes {
            let r = await kernel.send([Item("core", [
                    "type": "create_agent", "parent_id": "core", "handler_module": "sched_probe.tools", "id": .string(p),
                    "callable_by": ["sched": "*"],
                ])])[0].result
            #expect(r["error"].isNull, "\(r)")
            canCall[p] = "*"
        }
        let r = await kernel.send([Item("core", [
                "type": "create_agent", "parent_id": "core", "handler_module": "scheduler.tools", "id": "sched",
                "tick_sec": .double(0.1), "can_call": canCall,
            ])])[0].result
        #expect(r["error"].isNull, "\(r)")
        return (kernel, probe)
    }

    private func schedule(
        _ kernel: Kernel, _ target: String, interval: Int64 = 1, hit: [String: JSON] = [:],
        timeout: JSON? = nil
    ) async -> JSON {
        var hitPayload: JSON = ["type": "hit"]
        for (k, v) in hit { hitPayload[k] = v }
        var payload: JSON = [
            "type": "schedule", "target": .string(target), "interval_seconds": .integer(interval),
            "payload": hitPayload,
        ]
        if let timeout { payload["timeout"] = timeout }
        return await kernel.send([Item("sched", payload)])[0].result
    }

    private func waitFor(_ pred: () -> Bool, seconds: Double) async -> Bool {
        let deadline = Date().addingTimeInterval(seconds)
        while Date() < deadline {
            if pred() { return true }
            try? await Task.sleep(nanoseconds: 50_000_000)
        }
        return pred()
    }

    @Test func schedulerBootsWithoutAStoreAndScheduleFailfasts() async throws {
        // Boot never needs a store (python): the tick loop starts; the first
        // WRITE surfaces the loader's error, and nothing lingers.
        let kernel = makeKernelWithAll()
        _ = await kernel.send([Item("core", [
                "type": "create_agent", "parent_id": "core", "handler_module": "scheduler.tools", "id": "sched",
            ])])[0].result
        let boot = await kernel.send([Item("sched", ["type": "boot"])])[0].result
        #expect(boot["running"].asBool == true, "\(boot)")
        let r = await kernel.send([Item("sched", [
                "type": "schedule", "target": "core", "interval_seconds": .integer(5),
                "payload": ["type": "list_agents"] as JSON,
            ])])[0].result
        let err = r["error"].asString ?? ""
        #expect(err.hasPrefix("scheduler.schedule:"), "\(r)")
        #expect(err.contains("no store"), "\(r)")
        let list = await kernel.send([Item("sched", ["type": "list"])])[0].result
        #expect((list["schedules"].asArray ?? []).isEmpty)
        _ = await kernel.send([Item("sched", ["type": "shutdown"])])[0].result
    }

    @Test func dueSchedulesFireAsOneConcurrentBatch() async throws {
        // Two due schedules; the FIRST one blocks. Sequential firing cannot hit
        // the second until that block ends, so "the fast one landed while the
        // slow one was still inside its block" is the proof.
        //
        // The proof is a DEADLINE, not the gap between the two start stamps: the
        // gap is load-dependent and flaked under the full suite (0.41s against a
        // 0.25s tolerance, 2026-09-03) even though both fires were dispatched in
        // one batch. The block is 4s and the deadline 3s, so the margin is wide
        // enough that saturation cannot fake a pass.
        let fm = FileManager.default
        let rel = "fantastic-schedbatch-\(UUID().uuidString)"
        let dir = URL(fileURLWithPath: fm.currentDirectoryPath).appendingPathComponent(rel)
        try fm.createDirectory(at: dir, withIntermediateDirectories: true)
        defer { try? fm.removeItem(at: dir) }
        let (kernel, probe) = await makeProbedScheduler(dir: dir, rel: rel, probes: ["p_slow", "p_fast"])
        let r1 = await schedule(kernel, "p_slow", hit: ["block": .double(4.0)])
        let r2 = await schedule(kernel, "p_fast")
        #expect(r1["schedule_id"].asString != nil, "\(r1)")
        #expect(r2["schedule_id"].asString != nil, "\(r2)")
        _ = await kernel.send([Item("sched", ["type": "boot"])])[0].result
        // Anchor on the SLOW hit, never on boot: the first fire is due one
        // interval out, so a deadline measured from boot races the interval.
        let slowSeen = await waitFor({ probe.hit("p_slow") != nil }, seconds: 4.0)
        #expect(slowSeen, "the slow schedule must fire")
        // p_slow is now inside its 4s block (it records BEFORE sleeping).
        // Sequential firing could not reach p_fast until that block ends; the 3s
        // window leaves a full second of margin for a saturated executor.
        let fastEarly = await waitFor({ probe.hit("p_fast") != nil }, seconds: 3.0)
        #expect(fastEarly, "not one batch: the fast fire waited for the slow one")
        let both = await waitFor({ probe.hit("p_slow") != nil && probe.hit("p_fast") != nil }, seconds: 6.0)
        _ = await kernel.send([Item("sched", ["type": "shutdown"])])[0].result
        #expect(both, "both probes must be hit")
        let list = await kernel.send([Item("sched", ["type": "list"])])[0].result
        for s in list["schedules"].asArray ?? [] {
            #expect((s["run_count"].asInt ?? 0) >= 1, "\(s)")
        }
    }

    @Test func nextRunIsPersistedBeforeTheFire() async throws {
        // The probe reads schedules.json the moment it is hit: the store must
        // already hold the bumped run_count/next_run (persist-BEFORE-fire).
        let fm = FileManager.default
        let rel = "fantastic-schedpersist-\(UUID().uuidString)"
        let dir = URL(fileURLWithPath: fm.currentDirectoryPath).appendingPathComponent(rel)
        try fm.createDirectory(at: dir, withIntermediateDirectories: true)
        defer { try? fm.removeItem(at: dir) }
        let capture = dir.appendingPathComponent("agents/sched/schedules.json")
        let (kernel, probe) = await makeProbedScheduler(dir: dir, rel: rel, probes: ["p_cap"], capture: capture)
        let r = await schedule(kernel, "p_cap")
        let sid = try #require(r["schedule_id"].asString)
        _ = await kernel.send([Item("sched", ["type": "boot"])])[0].result
        let hit = await waitFor({ probe.hit("p_cap") != nil }, seconds: 3.0)
        _ = await kernel.send([Item("sched", ["type": "shutdown"])])[0].result
        #expect(hit, "probe must be hit")
        let seen = try #require(probe.hit("p_cap"))
        let doc = try #require(seen.doc, "store had no schedules.json at fire time")
        let rec = try #require((doc.asArray ?? []).first { $0["id"].asString == sid })
        #expect(rec["run_count"].asInt == 1, "\(rec)")
        #expect((rec["next_run"].asDouble ?? 0) > seen.at - 0.5, "\(rec) at \(seen.at)")
    }

    @Test func aScheduleTimeoutBoundsItsFire() async throws {
        let fm = FileManager.default
        let rel = "fantastic-schedtimeout-\(UUID().uuidString)"
        let dir = URL(fileURLWithPath: fm.currentDirectoryPath).appendingPathComponent(rel)
        try fm.createDirectory(at: dir, withIntermediateDirectories: true)
        defer { try? fm.removeItem(at: dir) }
        let (kernel, _) = await makeProbedScheduler(dir: dir, rel: rel, probes: ["p_hang"])
        let r = await schedule(kernel, "p_hang", interval: 60, hit: ["block": .double(2.0)], timeout: .double(0.1))
        let sid = try #require(r["schedule_id"].asString)
        #expect(r["schedule"]["timeout"].asDouble == 0.1, "\(r)")
        let t0 = Date()
        let fired = await kernel.send([Item("sched", ["type": "tick_now", "schedule_id": .string(sid)])])[0].result
        #expect(fired["fired"].asBool == true, "\(fired)")
        #expect(Date().timeIntervalSince(t0) < 1.5, "fire was not bounded by the schedule timeout")
        let h = await kernel.send([Item("sched", ["type": "history", "schedule_id": .string(sid)])])[0].result
        #expect(h["count"].asInt == 1, "\(h)")
        #expect((h["history"][0]["error"].asString ?? "").contains("timed_out"), "\(h)")
        // A bad timeout is refused with the kernel's bad_arg code.
        for badValue in [JSON.integer(0), .integer(-1), .string("5"), .bool(true)] {
            let bad = await schedule(kernel, "p_hang", timeout: badValue)
            #expect(bad["reason"].asString == "bad_arg" && bad["arg"].asString == "timeout", "\(bad)")
        }
        _ = await kernel.send([Item("sched", ["type": "shutdown"])])[0].result
    }

    @Test func nonIntArgsAreBadArgRefusals() async throws {
        // #136 — a present-but-wrong number is a TYPO, not a new default.
        // `asInt ?? 60` used to swallow `interval_seconds: "60"` and schedule
        // hourly what the caller wrote as per-minute.
        let fm = FileManager.default
        let rel = "fantastic-schedints-\(UUID().uuidString)"
        let dir = URL(fileURLWithPath: fm.currentDirectoryPath).appendingPathComponent(rel)
        try fm.createDirectory(at: dir, withIntermediateDirectories: true)
        defer { try? fm.removeItem(at: dir) }
        let (kernel, _) = await makeProbedScheduler(dir: dir, rel: rel, probes: ["p_x"])
        let badIntervals: [JSON] = [
            .string("60"), .double(1.5), .bool(true), .null, .integer(0), .integer(-1),
            .array([.integer(60)]),
        ]
        for badValue in badIntervals {
            let r = await kernel.send([Item("sched", [
                    "type": "schedule", "target": "p_x", "payload": ["type": "hit"] as JSON,
                    "interval_seconds": badValue,
                ])])[0].result
            #expect(r["reason"].asString == "bad_arg", "\(badValue.serialize()): \(r.serialize())")
            #expect(r["arg"].asString == "interval_seconds", "\(badValue.serialize()): \(r.serialize())")
        }
        // Nothing was scheduled by a refused call.
        let listed = await kernel.send([Item("sched", ["type": "list"])])[0].result
        #expect((listed["schedules"].asArray ?? []).isEmpty, "\(listed.serialize())")
        // ABSENT still takes the documented default (60s), and is NOT refused.
        let ok = await kernel.send([Item("sched", [
                "type": "schedule", "target": "p_x", "payload": ["type": "hit"] as JSON,
            ])])[0].result
        #expect(ok["schedule"]["interval_seconds"].asInt == 60, "\(ok.serialize())")

        for badValue in [JSON.string("100"), .double(2.0), .bool(true), .null, .integer(0)] {
            let h = await kernel.send([Item("sched", ["type": "history", "limit": badValue])])[0].result
            #expect(h["reason"].asString == "bad_arg", "\(badValue.serialize()): \(h.serialize())")
            #expect(h["arg"].asString == "limit", "\(badValue.serialize()): \(h.serialize())")
        }
        // ABSENT `limit` is the default page, not a refusal.
        let h = await kernel.send([Item("sched", ["type": "history"])])[0].result
        #expect(!h["history"].isNull && h["reason"].isNull, "\(h.serialize())")
        _ = await kernel.send([Item("sched", ["type": "shutdown"])])[0].result
    }

    @Test func scheduleRefusesANonObjectPayload() async throws {
        // #136 — `payload: [1]` can never carry a `type`; say so as an ARG error.
        let fm = FileManager.default
        let rel = "fantastic-schedpayload-\(UUID().uuidString)"
        let dir = URL(fileURLWithPath: fm.currentDirectoryPath).appendingPathComponent(rel)
        try fm.createDirectory(at: dir, withIntermediateDirectories: true)
        defer { try? fm.removeItem(at: dir) }
        let (kernel, _) = await makeProbedScheduler(dir: dir, rel: rel, probes: ["p_x"])
        for badValue in [JSON.array([.integer(1)]), .string("hit"), .integer(7)] {
            let r = await kernel.send([Item("sched", [
                    "type": "schedule", "target": "p_x", "payload": badValue,
                ])])[0].result
            #expect(r["reason"].asString == "bad_arg", "\(badValue.serialize()): \(r.serialize())")
            #expect(r["arg"].asString == "payload", "\(badValue.serialize()): \(r.serialize())")
        }
        _ = await kernel.send([Item("sched", ["type": "shutdown"])])[0].result
    }

    @Test func aNonNumericTickSecIsRefusedAtWriteTime() async {
        // #136 — `tick_sec: "0.5"` was silently defaulted to 1.0. The record is
        // judged where the mistake is made.
        let kernel = makeKernelWithAll()
        let r = await kernel.send([Item("core", [
                "type": "create_agent", "parent_id": "core",
                "handler_module": "scheduler.tools", "id": "sch_ticks", "tick_sec": "0.5",
            ])])[0].result
        #expect(r["reason"].asString == "invalid_meta", "\(r.serialize())")
        #expect((r["error"].asString ?? "").contains("tick_sec"), "\(r.serialize())")
        #expect(kernel.agent(AgentId("sch_ticks")) == nil)
    }

    @Test func kernelBridgeForwardInMemory() async {
        let kernel1 = makeKernelWithAll()
        let kernel2 = makeKernelWithAll()
        // Wire bridge from kernel1 → kernel2.
        let bridge = KernelBridgeBundle()
        kernel1.bundles.register("ws_bridge.tools", bridge)
        _ = await kernel1.send([Item("core", ["type": "create_agent", "parent_id": "core", "handler_module": "ws_bridge.tools", "id": "br"])])[0].result
        bridge.attachInMemory(agentId: "br", remote: kernel2, localKernel: kernel1)
        let reply = await kernel1.send([Item("br", [
                "type": "forward",
                "target": "core",
                "payload": ["type": "list_agents"] as JSON,
            ])])[0].result
        // kernel2's list_agents returns "agents" array including core.
        let names = (reply["agents"].asArray ?? []).map { $0["id"].asString ?? "" }
        #expect(names.contains("core"))
    }

    @Test func kernelBridgeBinaryForwardStreamsRawBytes() async throws {
        // Cross-kernel STREAMING — forward a write_stream/read_stream over the
        // (in-memory) bridge to a remote file_bridge; raw bytes both ways.
        let kernel1 = makeKernelWithAll()
        let kernel2 = makeKernelWithAll()
        let bridge = KernelBridgeBundle()
        kernel1.bundles.register("ws_bridge.tools", bridge)
        _ = await kernel1.send([Item("core", ["type": "create_agent", "parent_id": "core", "handler_module": "ws_bridge.tools", "id": "br"])])[0].result
        bridge.attachInMemory(agentId: "br", remote: kernel2, localKernel: kernel1)
        // kernel2: an OPEN file_bridge in a cwd-relative dir.
        let fm = FileManager.default
        let rel = "fantastic-brstream-\(UUID().uuidString)"
        let dir = URL(fileURLWithPath: fm.currentDirectoryPath).appendingPathComponent(rel)
        try fm.createDirectory(at: dir, withIntermediateDirectories: true)
        defer { try? fm.removeItem(at: dir) }
        _ = await kernel2.send([Item("core", [
                "type": "create_agent", "parent_id": "core", "handler_module": "file_bridge.tools", "id": "fs",
                "root": .string(rel), "ingress_rule": "allow_all_ingress",
            ])])[0].result
        let payload = Data([0x00, 0xFF, 0xCA, 0xFE, 0xBA, 0xBE, 0x10, 0x80])
        // forward write_stream (binary) → kernel2.fs
        let (w, _) = await kernel1.sendWithBinary(
            "br",
            .object([
                "type": .string("forward"), "target": .string("fs"),
                "payload": .object([
                    "type": .string("write_stream"), "path": .string("blob.bin"),
                    "truncate": .bool(true),
                ]),
            ]), payload)
        #expect(w["written"].asInt == Int64(payload.count), "\(w)")
        // forward read_stream (binary) → bytes back over the bridge
        let (_, body) = await kernel1.sendWithBinary(
            "br",
            .object([
                "type": .string("forward"), "target": .string("fs"),
                "payload": .object([
                    "type": .string("read_stream"), "path": .string("blob.bin"),
                ]),
            ]), Data())
        #expect(body == payload, "raw bytes must round-trip cross-kernel over the bridge")
    }

    @Test func wsBinaryForwardStreamsRawBytesOverWire() async throws {
        // The WIRE half of cross-kernel streaming — a bridge's
        // `WebSocketTransport.binaryForward` ships a codec binary frame over a
        // real loopback WS to a remote `web_ws`, which decodes it, gates, and
        // dispatches `sendWithBinary` to a `file_bridge`. Raw bytes both ways,
        // never base64. (The in-memory variant is
        // `kernelBridgeBinaryForwardStreamsRawBytes`.)
        let kernel = try await startKernelInMemory(portHint: 0)
        _ = await kernel.send([Item(AgentId("web"), .object(["type": .string("boot")]))])[0].result
        // web_ws SEALS by default — open it for the inbound binary call.
        let rec = await kernel.send([Item(AgentId("kernel"), .object([
                "type": .string("create_agent"),
                "parent_id": .string("web"),
                "handler_module": .string("web_ws.tools"),
                "id": .string("web_ws"),
                "ingress_rule": .string("allow_all_ingress"),
                // The EGRESS half. An inbound frame dispatches AS THE LEG now, and
                // `fs` is the leg's cousin (child of core, while the leg is a child
                // of web) — no structural baseline token reaches it.
                "can_call": .object(["fs": .string("*")]),
            ]))])[0].result
        let wsId = rec["id"].asString ?? "web_ws"
        _ = await kernel.send([Item(AgentId("web"), .object(["type": .string("mount"), "child_id": .string(wsId)]))])[0].result
        defer {
            Task {
                _ = await kernel.send([Item(AgentId("web"), .object(["type": .string("shutdown")]))])[0].result
            }
        }
        // An OPEN file_bridge in a cwd-relative dir on the SAME kernel.
        let fm = FileManager.default
        let rel = "fantastic-wsbrstream-\(UUID().uuidString)"
        let dir = URL(fileURLWithPath: fm.currentDirectoryPath)
            .appendingPathComponent(rel)
        try fm.createDirectory(at: dir, withIntermediateDirectories: true)
        defer { try? fm.removeItem(at: dir) }
        _ = await kernel.send([Item(AgentId("core"), .object([
                "type": .string("create_agent"),
                "parent_id": .string("core"),
                "handler_module": .string("file_bridge.tools"),
                "id": .string("fs"), "root": .string(rel),
                "ingress_rule": .string("allow_all_ingress"),
                // …and the INGRESS half, on the target.
                "callable_by": .object([
                    "$self": .string("*"), "$parent": .string("*"),
                    "$children": .string("*"), "web_ws": .string("*"),
                ]),
            ]))])[0].result

        let port = kernel.httpPort()
        let url = URL(string: "ws://127.0.0.1:\(port)/core/ws")!
        let transport = WebSocketTransport(endpoint: url)
        await transport.connect()

        let payload = Data([0x00, 0xFF, 0xCA, 0xFE, 0xBA, 0xBE, 0x10, 0x80])
        // write_stream (binary) over the wire → kernel.fs
        let (w, _) = await transport.binaryForward(
            target: AgentId("fs"),
            header: .object([
                "type": .string("write_stream"), "path": .string("blob.bin"),
                "truncate": .bool(true),
            ]),
            blob: payload)
        #expect(w["written"].asInt == Int64(payload.count), "\(w)")
        // read_stream (binary) over the wire → raw bytes back
        let (_, body) = await transport.binaryForward(
            target: AgentId("fs"),
            header: .object([
                "type": .string("read_stream"), "path": .string("blob.bin"),
            ]),
            blob: Data())
        #expect(body == payload, "raw bytes must round-trip over the WS wire")
    }

    @Test func kernelBridgeWatchRemoteReEmitsOnLocalInbox() async {
        // Two kernels in process. A's bridge subscribes to B.core's
        // emits via watch_remote. We trigger an emit on B.core, then
        // verify the payload arrives on A's bridge inbox via a
        // synthetic watcher.
        let kernelA = makeKernelWithAll()
        let kernelB = makeKernelWithAll()
        let bridge = KernelBridgeBundle()
        kernelA.bundles.register("ws_bridge.tools", bridge)
        _ = await kernelA.send([Item("core", ["type": "create_agent", "parent_id": "core", "handler_module": "ws_bridge.tools", "id": "br"])])[0].result
        bridge.attachInMemory(agentId: "br", remote: kernelB, localKernel: kernelA)

        // Give attachInMemory's async setLocalSink Task a tick to land.
        try? await Task.sleep(nanoseconds: 50_000_000)

        // Subscribe via the bridge: A.br.watch_remote(target=core)
        let w = await kernelA.send([Item("br", ["type": "watch_remote", "target": "core"])])[0].result
        #expect(w["ok"].asBool == true)

        // Synthetic local watcher of the bridge agent's inbox. Every
        // re-emitted event lands here.
        let watcherId: AgentId = "test_local_watcher"
        // THE WATCHER REGISTERS FIRST — `watch` mirrors into an inbox, it does
        // not create one on the caller's behalf.
        let inbox = kernelA.ensureInbox(watcherId)
        kernelA.watch(src: "br", watcher: watcherId)

        // Trigger an emit on B.core. The substrate's `emit` fans out
        // to watcher inboxes — our synthetic in-memory relay drains
        // the watcher inbox and re-emits on A.br, which fans out to
        // the local watcher.
        await kernelB.emit(
            "core",
            ["type": "token", "text": "hi"])

        // Wait for the event with a short timeout.
        let event = await withTaskGroup(of: JSON?.self) { group in
            group.addTask {
                for await ev in inbox {
                    if ev["type"].asString == "token" { return ev }
                }
                return nil
            }
            group.addTask {
                try? await Task.sleep(nanoseconds: 1_000_000_000)
                return nil
            }
            let first = await group.next() ?? nil
            group.cancelAll()
            return first
        }
        #expect(event != nil, "expected re-emitted token event on bridge inbox")
        #expect(event?["text"].asString == "hi")

        _ = await kernelA.send([Item("br", ["type": "unwatch_remote", "target": "core"])])[0].result
    }

    @Test func cliRendererAttaches() async {
        let kernel = makeKernelWithAll()
        let token = attach(kernel)
        // Just verify the token works for unsubscribe — full CLI
        // output testing is overkill for a smoke test.
        kernel.unsubscribe(token)
    }

    @Test func webWSGetRoutesShape() async {
        let kernel = makeKernelWithAll()
        _ = await kernel.send([Item("core", ["type": "create_agent", "parent_id": "core", "handler_module": "web_ws.tools", "id": "ws"])])[0].result
        let r = await kernel.send([Item("ws", ["type": "get_routes"])])[0].result
        let routes = r["routes"].asArray ?? []
        #expect(routes.count == 1)
        #expect(routes.first?["kind"].asString == "websocket")
        #expect(routes.first?["path"].asString == "/{host_id}/ws")
    }

    @Test func webRestGetRoutesNamespacedBySelf() async {
        let kernel = makeKernelWithAll()
        _ = await kernel.send([Item("core", ["type": "create_agent", "parent_id": "core", "handler_module": "web_rest.tools", "id": "rest"])])[0].result
        let r = await kernel.send([Item("rest", ["type": "get_routes"])])[0].result
        let paths = (r["routes"].asArray ?? []).compactMap { $0["path"].asString }
        #expect(paths.contains("/rest/{target}"))
        #expect(paths.contains("/rest/_reflect"))
        #expect(paths.contains("/rest/_reflect/{target}"))
    }

    @Test func webRestHandleRoutePostDispatchesBodyVerb() async {
        let kernel = makeKernelWithAll()
        // The web_rest leg SEALS by default — open it for the inbound POST.
        _ = await kernel.send([Item("core", [
                "type": "create_agent", "parent_id": "core", "handler_module": "web_rest.tools", "id": "rest",
                "ingress_rule": "allow_all_ingress",
            ])])[0].result
        // Simulate the host calling handle_route for
        // POST /rest/core  body={type:list_agents}.
        let reply = await kernel.send([Item("rest", [
                "type": "handle_route",
                "method": "POST",
                "params": ["target": "core"] as JSON,
                "query": [:] as JSON,
                "body": "{\"type\":\"list_agents\"}",
            ])])[0].result
        #expect(reply["status"].asInt == 200)
        #expect(reply["content_type"].asString == "application/json")
        let body = reply["body"].asString ?? ""
        #expect(body.contains("\"core\""))
    }

    @Test func webRestHandleRouteBadJsonIs400() async {
        let kernel = makeKernelWithAll()
        _ = await kernel.send([Item("core", ["type": "create_agent", "parent_id": "core", "handler_module": "web_rest.tools", "id": "rest"])])[0].result
        let reply = await kernel.send([Item("rest", [
                "type": "handle_route",
                "method": "POST",
                "params": ["target": "core"] as JSON,
                "query": [:] as JSON,
                "body": "not json",
            ])])[0].result
        #expect(reply["status"].asInt == 400)
    }
}

/// A probe target for the scheduler tests: `hit` records when it ran (and the
/// scheduler's persisted `schedules.json` at that instant when `capture` is
/// set), optionally blocking `block` seconds.
final class SchedProbe: AgentBundle, @unchecked Sendable {
    struct Seen {
        let at: Double
        let doc: JSON?
    }
    let name = "sched_probe"
    private let lock = NSLock()
    private var hits: [String: Seen] = [:]
    private let capture: URL?

    init(capture: URL?) { self.capture = capture }

    private func record(_ id: String, _ seen: Seen) {
        lock.lock()
        defer { lock.unlock() }
        hits[id] = seen
    }

    func hit(_ id: String) -> Seen? {
        lock.lock()
        defer { lock.unlock() }
        return hits[id]
    }

    func handle(agentId: AgentId, payload: JSON, kernel: Kernel) async throws -> JSON? {
        switch payload["type"].asString ?? "" {
        case "boot", "shutdown": return nil
        case "reflect":
            return .object([
                "id": .string(agentId.value), "sentence": .string("scheduler probe"),
                "verbs": .object([:]),
            ])
        case "hit":
            var doc: JSON? = nil
            if let capture, let raw = try? String(contentsOf: capture, encoding: .utf8) {
                doc = try? JSON.parse(raw)
            }
            record(agentId.value, Seen(at: Date().timeIntervalSince1970, doc: doc))
            if let block = payload["block"].asDouble {
                try await Task.sleep(nanoseconds: UInt64(block * 1_000_000_000))
            }
            return .object(["hit": .string(agentId.value)])
        default:
            return .object(["error": .string("unknown verb")])
        }
    }
}
