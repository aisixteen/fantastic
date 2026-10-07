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
import FantasticScheduler
import FantasticWeb
import FantasticWebRest
import FantasticWebWS
import Foundation
import FantasticKernelState
import Testing

func makeKernelWithAll(
    rootPath: URL = URL(fileURLWithPath: FileManager.default.currentDirectoryPath)
        .appendingPathComponent(".fantastic")
) -> Kernel {
    let registry = BundleRegistry()
    // THE ROOT IS THE LOADER, as in every real kernel: `persist_blob` /
    // `load_blob` are its verbs (C13), and the root reflect frame is its
    // `reflectRootExtra`.
    registry.register(ROOT_HANDLER_MODULE, KernelStateBundle())
    registry.register("file_bridge.tools", FileBundle())
    registry.register(CliBundle.handlerModule, CliBundle())
    registry.register("scheduler.tools", SchedulerBundle())
    registry.register("ws_bridge.tools", KernelBridgeBundle())
    registry.register("web_ws.tools", WebWSBundle())
    registry.register("web_rest.tools", WebRestBundle())
    let kernel = Kernel(storage: .inMemory, bundles: registry)
    // The root's own dir IS the store root: `findStore` discovers a
    // `file_bridge` child whose `root` resolves to it, which is how the loader
    // owns where a sidecar lands.
    let root = Agent(
        id: AgentId(ROOT_ID), handlerModule: ROOT_HANDLER_MODULE, parentId: nil,
        rootPath: rootPath)
    kernel.register(root)
    kernel.setRoot(root)
    return kernel
}

/// Python `_wire_memory_pair`: leg `br` on `k1` (guid `guid-a`) and a SERVING leg
/// `br_peer` on `k2` (guid `guid-b`, opened, pinned to `guid-a`), joined by a
/// memory pair and booted; `k2`'s root accepts `guid-a`'s agents. A forward from
/// `br` is served by `br_peer` — gated, pinned, the target judging the caller as
/// `guid-a|<agent>` — never by `k2`'s operator.
func wireMemoryPair(_ k1: Kernel, _ k2: Kernel) async {
    let b1 = KernelBridgeBundle()
    k1.bundles.register("ws_bridge.tools", b1)
    let b2 = KernelBridgeBundle()
    k2.bundles.register("ws_bridge.tools", b2)
    k1.update(AgentId(ROOT_ID), ["guid": "guid-a"])
    k2.update(AgentId(ROOT_ID), ["guid": "guid-b"])
    // a call the far leg serves is judged as THAT LEG: the root accepts it
    var cb = k2.agent(AgentId(ROOT_ID))?.meta["callable_by"]?.asObject ?? [:]
    cb["br_peer"] = "*"
    k2.update(AgentId(ROOT_ID), ["callable_by": .object(cb)])
    _ = await k1.send([Item("kernel_state", [
            "type": "create_agent", "parent_id": "kernel_state", "handler_module": "ws_bridge.tools",
            "id": "br", "transport": "memory",
        ])])[0].result
    _ = await k2.send([Item("kernel_state", [
            "type": "create_agent", "parent_id": "kernel_state", "handler_module": "ws_bridge.tools",
            "id": "br_peer", "transport": "memory",
            "ingress_rule": "allow_all_ingress",
            // the leg's reach is explicit: the root and its own children
            "can_call": ["$subtree": "*", "kernel_state": "*"],
        ])])[0].result
    let (t1, t2) = MemoryTransport.pair()
    b1.injectTransport(agentId: "br", transport: t1)
    b2.injectTransport(agentId: "br_peer", transport: t2)
    for (k, id) in [(k1, "br"), (k2, "br_peer")] {
        let r = await k.send([Item(AgentId(id), ["type": "boot"])])[0].result
        #expect(r["booted"] == true, "\(r.serialize())")
    }
}

/// A disk-mode kernel rooted at `workdir/.fantastic`, with the full registry —
/// for the persistence-inversion test (records persist THROUGH a file_bridge).
func makeDiskKernel(workdir: URL) -> Kernel {
    let registry = BundleRegistry()
    registry.register(ROOT_HANDLER_MODULE, KernelStateBundle())
    registry.register("file_bridge.tools", FileBundle())
    registry.register(CliBundle.handlerModule, CliBundle())
    let kernel = Kernel(storage: .disk(workdir), bundles: registry)
    let root = Agent(
        id: AgentId(ROOT_ID), handlerModule: ROOT_HANDLER_MODULE, parentId: nil,
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
        // RECORDS PERSIST THROUGH THE ROOT LOADER'S WRITER now (C14), not at the
        // verb — so the writer is installed and each assertion drains it. That
        // drain is the one-shot seam python's `call` mode uses
        // (`root._fs_flush_loop.aflush()`), not a test-only shortcut.
        kernel.installPersistWriter(loader: kernel.root!.id)
        func file(_ rel: String) -> String {
            workdir.appendingPathComponent(".fantastic/\(rel)").path
        }
        /// The writer's debounced task and an explicit drain can be mid-pass at
        /// the same time, so a drain is a lower bound, not a barrier — wait for
        /// the effect rather than assuming one pass did it.
        func settle(_ rel: String, exists: Bool) async -> Bool {
            for _ in 0..<100 {
                if fm.fileExists(atPath: file(rel)) == exists { return true }
                await kernel.flushRecords()
                try? await Task.sleep(nanoseconds: 20_000_000)
            }
            return fm.fileExists(atPath: file(rel)) == exists
        }

        // No store wired ⇒ the create stays in RAM (NO direct-fs fallback).
        _ = await kernel.send([Item("kernel_state", ["type": "create_agent", "parent_id": "kernel_state", "handler_module": "cli.tools", "id": "ram_only"])])[0].result
        await kernel.flushRecords()
        #expect(!fm.fileExists(atPath: file("agents/ram_only/agent.yaml")))
        #expect(kernel.agent("ram_only") != nil)  // live in RAM

        // Wire the store (file_bridge @ .fantastic, open) — it self-persists.
        _ = await kernel.send([Item("kernel_state", [
                "type": "create_agent", "parent_id": "kernel_state", "handler_module": "file_bridge.tools", "allow": ["."], "id": "store",
                "root": ".fantastic", "ingress_rule": "allow_all_ingress",
            ])])[0].result
        #expect(
            await settle("agents/store/agent.yaml", exists: true),
            "the store must persist itself through itself")

        // Now a created agent persists THROUGH the provider.
        _ = await kernel.send([Item("kernel_state", [
                "type": "create_agent", "parent_id": "kernel_state", "handler_module": "cli.tools", "id": "foo",
                "display_name": "Foo",
            ])])[0].result
        #expect(await settle("agents/foo/agent.yaml", exists: true))
        let content = try String(contentsOf: URL(fileURLWithPath: file("agents/foo/agent.yaml")))
        #expect(content.hasPrefix("id: foo\n") && content.contains("display_name: Foo\n"), "\(content)")

        // Delete removes the record THROUGH the provider's recursive delete.
        _ = await kernel.send([Item("kernel_state", ["type": "delete_agent", "id": "foo"])])[0].result
        #expect(await settle("agents/foo", exists: false))
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
        _ = await kernel.send([Item("kernel_state", [
                "type": "create_agent", "parent_id": "kernel_state", "handler_module": "file_bridge.tools", "allow": ["."], "id": "fs",
                "root": .string(rel), "ingress_rule": "allow_all_ingress",
            ])])[0].result
        // Non-UTF-8 bytes — proves the channel is raw, not text/base64.
        let payload = Data([0x00, 0xFF, 0xCA, 0xFE, 0xBA, 0xBE, 0x10, 0x80])
        // SINK.
        let w = await kernel.send([
            Item(
                "fs",
                .object([
                    "type": .string("write_stream"), "path": .string("blob.bin"),
                    "truncate": .bool(true),
                ]), binary: Blob("bytes", payload))
        ])[0].result
        #expect(w["written"].asInt == Int64(payload.count), "\(w)")
        // SOURCE.
        let read = await kernel.send([
            Item(
                "fs", .object(["type": .string("read_stream"), "path": .string("blob.bin")]),
                rawReply: true)
        ])[0]
        let (m, body) = (read.result, read.body)
        #expect(body == Blob("bytes", payload), "bytes must round-trip verbatim, at `bytes`")
        #expect(m["eof"].asBool == true)
        #expect(m["size"].asInt == Int64(payload.count))
    }

    @Test func fileBundleReflect() async throws {
        let kernel = makeKernelWithAll()
        let tmp = FileManager.default.temporaryDirectory.appendingPathComponent(
            "fantastic-file-test-\(UUID().uuidString)")
        try? FileManager.default.createDirectory(at: tmp, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: tmp) }
        _ = await kernel.send([Item("kernel_state", [
                "type": "create_agent",
                "parent_id": "kernel_state",
                "handler_module": "file_bridge.tools", "allow": ["."],
                "id": "fs1",
                "root": .string(tmp.path),
            ])])[0].result
        let r = await kernel.send([Item("fs1", ["type": "reflect"])])[0].result
        #expect(r["kind"].asString == nil || r["sentence"].asString?.contains("Filesystem") == true)
    }

    @Test func schedulerScheduleListUnschedule() async throws {
        // py/rust contract: schedule {target, payload, interval_seconds} → mints a
        // schedule_id, persisted THROUGH the loader (store-relative
        // agents/<id>/schedules.yaml); a WRITE failfasts until a store is wired.
        let fm = FileManager.default
        let rel = "fantastic-sched-\(UUID().uuidString)"
        let dir = URL(fileURLWithPath: fm.currentDirectoryPath).appendingPathComponent(rel)
        try fm.createDirectory(at: dir, withIntermediateDirectories: true)
        let kernel = makeKernelWithAll(rootPath: dir)
        defer { try? fm.removeItem(at: dir) }
        // Open file_bridge store — the scheduler persists through it.
        _ = await kernel.send([Item("kernel_state", [
                "type": "create_agent", "parent_id": "kernel_state", "handler_module": "file_bridge.tools", "allow": ["."], "id": "store",
                "root": .string(rel), "ingress_rule": "allow_all_ingress",
                // Two-sided deny-all: accept the scheduler that persists through it.
                "callable_by": ["sched": "*"],
            ])])[0].result
        _ = await kernel.send([Item("kernel_state", [
                "type": "create_agent", "parent_id": "kernel_state", "handler_module": "scheduler.tools", "id": "sched",
                "can_call": ["store": "*"],
            ])])[0].result
        let r = await kernel.send([Item("sched", [
                "type": "schedule", "target": "kernel_state", "interval_seconds": .integer(60),
                "payload": ["type": "list_agents"] as JSON,
            ])])[0].result
        let sid = r["schedule_id"].asString
        #expect(sid != nil, "\(r)")
        // store-relative sidecar landed under the store root.
        #expect(
            fm.fileExists(atPath: dir.appendingPathComponent("agents/sched/schedules.yaml").path))
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
        // schedule_events.yaml sidecar (through the provider), and `history` reads it
        // back. Mirrors the py/rust contract.
        let fm = FileManager.default
        let rel = "fantastic-schedfire-\(UUID().uuidString)"
        let dir = URL(fileURLWithPath: fm.currentDirectoryPath).appendingPathComponent(rel)
        try fm.createDirectory(at: dir, withIntermediateDirectories: true)
        let kernel = makeKernelWithAll(rootPath: dir)
        defer { try? fm.removeItem(at: dir) }
        _ = await kernel.send([Item("kernel_state", [
                "type": "create_agent", "parent_id": "kernel_state", "handler_module": "file_bridge.tools", "allow": ["."], "id": "store",
                "root": .string(rel), "ingress_rule": "allow_all_ingress",
                // Two-sided deny-all: accept the scheduler that persists through it.
                "callable_by": ["sched": "*"],
            ])])[0].result
        _ = await kernel.send([Item("kernel_state", [
                "type": "create_agent", "parent_id": "kernel_state", "handler_module": "scheduler.tools", "id": "sched",
                "can_call": ["store": "*"],
            ])])[0].result
        wire(kernel, "sched", "kernel_state")  // a schedule fires AS the scheduler
        let r = await kernel.send([Item("sched", [
                "type": "schedule", "target": "kernel_state", "interval_seconds": .integer(3600),
                "payload": ["type": "list_agents"] as JSON,
            ])])[0].result
        let sid = try #require(r["schedule_id"].asString)
        // Fire it NOW (no tick-loop wait).
        let fired = await kernel.send([Item("sched", ["type": "tick_now", "schedule_id": .string(sid)])])[0].result
        #expect(fired["fired"].asBool == true, "\(fired)")
        // the reply carries the fire's OUTCOME — the target's own answer
        #expect(!fired["result"]["agents"].isNull && fired["error"].isNull, "\(fired)")
        // A target that refuses: `{fired:true}` alone hid sixteen refusals from the
        // model (measured 2026-10-06, rust subai_graph). Python
        // `test_tick_now_reports_a_refused_fire`.
        let g = await kernel.send([Item("sched", [
                "type": "schedule", "target": "ghost", "interval_seconds": .integer(3600),
                "payload": ["type": "tick"] as JSON,
            ])])[0].result
        let ghost = await kernel.send([Item("sched", [
                "type": "tick_now", "schedule_id": .string(g["schedule_id"].asString ?? ""),
            ])])[0].result
        let said = ghost["error"].isNull ? ghost["result"]["error"] : ghost["error"]
        #expect(pyStr(said).contains("ghost"), "\(ghost)")
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
        // The history sidecar landed store-relative, next to schedules.yaml.
        #expect(
            fm.fileExists(atPath: dir.appendingPathComponent("agents/sched/schedule_events.yaml").path))
    }

    // ── #82 / #145: boot needs no store; one concurrent batch per tick,
    //    persist-before-fire, per-schedule timeout ──────────────────

    /// A store-backed kernel with `probes` probe targets and a scheduler
    /// (tick 0.1s) granted to call them — both sides of the deny-all gate open.
    private func makeProbedScheduler(
        dir: URL, rel: String, probes: [String], capture: URL? = nil, tickSec: Double = 0.1,
        paused: Bool = false
    ) async -> (Kernel, SchedProbe) {
        let kernel = makeKernelWithAll(rootPath: dir)
        let probe = SchedProbe(capture: capture)
        kernel.bundles.register("sched_probe.tools", probe)
        _ = await kernel.send([Item("kernel_state", [
                "type": "create_agent", "parent_id": "kernel_state", "handler_module": "file_bridge.tools", "allow": ["."], "id": "store",
                "root": .string(rel), "ingress_rule": "allow_all_ingress",
            ])])[0].result
        var canCall: JSON = .object([:])
        for p in probes {
            let r = await kernel.send([Item("kernel_state", [
                    "type": "create_agent", "parent_id": "kernel_state", "handler_module": "sched_probe.tools", "id": .string(p),
                    "callable_by": ["sched": "*"],
                ])])[0].result
            #expect(r["error"].isNull, "\(r)")
            canCall[p] = "*"
        }
        let r = await kernel.send([Item("kernel_state", [
                "type": "create_agent", "parent_id": "kernel_state", "handler_module": "scheduler.tools", "id": "sched",
                "tick_sec": .double(tickSec), "can_call": canCall, "paused": .bool(paused),
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
        _ = await kernel.send([Item("kernel_state", [
                "type": "create_agent", "parent_id": "kernel_state", "handler_module": "scheduler.tools", "id": "sched",
            ])])[0].result
        let boot = await kernel.send([Item("sched", ["type": "boot"])])[0].result
        #expect(boot["running"].asBool == true, "\(boot)")
        let r = await kernel.send([Item("sched", [
                "type": "schedule", "target": "kernel_state", "interval_seconds": .integer(5),
                "payload": ["type": "list_agents"] as JSON,
            ])])[0].result
        let err = r["error"].asString ?? ""
        #expect(err.hasPrefix("scheduler.schedule:"), "\(r)")
        #expect(err.contains("no store"), "\(r)")
        let list = await kernel.send([Item("sched", ["type": "list"])])[0].result
        #expect((list["schedules"].asArray ?? []).isEmpty)
        _ = await kernel.send([Item("kernel", ["type": "delete_agent", "id": "sched"])])[0].result
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
        // A 2s SWEEP against a 1s interval, so both schedules are OVERDUE when
        // the first sweep comes and the sweep is the batch. `next_run` is
        // `created + interval`, and `create_agent` boots the scheduler, so the
        // loop is already sweeping while the two `schedule` calls run: with a
        // 0.1s sweep the wall gap between them (a store write each, 38 ms
        // measured under the full parallel run) landed a sweep BETWEEN their
        // two `next_run`s. p_slow then fired alone and held the loop for its
        // whole 4s block, and the batch proof failed on scheduling rather than
        // on behaviour. The 2s sweep absorbs any gap under a second.
        let (kernel, probe) = await makeProbedScheduler(
            dir: dir, rel: rel, probes: ["p_slow", "p_fast"], tickSec: 2.0)
        let r1 = await schedule(kernel, "p_slow", hit: ["block": .double(4.0)])
        let r2 = await schedule(kernel, "p_fast")
        #expect(r1["schedule_id"].asString != nil, "\(r1)")
        #expect(r2["schedule_id"].asString != nil, "\(r2)")
        _ = await kernel.send([Item("sched", ["type": "boot"])])[0].result
        // Anchor on the SLOW hit, never on boot: the first fire is due one
        // interval out, so a deadline measured from boot races the interval.
        let slowSeen = await waitFor({ probe.hit("p_slow") != nil }, seconds: 6.0)
        #expect(slowSeen, "the slow schedule must fire")
        // p_slow is now inside its 4s block (it records BEFORE sleeping).
        // Sequential firing could not reach p_fast until that block ends; the 3s
        // window leaves a full second of margin for a saturated executor.
        let fastEarly = await waitFor({ probe.hit("p_fast") != nil }, seconds: 3.0)
        let dbgList = await kernel.send([Item("sched", ["type": "list"])])[0].result
        let dbg =
            "now=\(Date().timeIntervalSince1970) slow=\(probe.hit("p_slow")?.at ?? -1) "
            + "fast=\(probe.hit("p_fast")?.at ?? -1) list=\(dbgList.serialize())"
        #expect(fastEarly, "not one batch: the fast fire waited for the slow one; \(dbg)")
        let both = await waitFor({ probe.hit("p_slow") != nil && probe.hit("p_fast") != nil }, seconds: 6.0)
        _ = await kernel.send([Item("kernel", ["type": "delete_agent", "id": "sched"])])[0].result
        #expect(both, "both probes must be hit")
        let list = await kernel.send([Item("sched", ["type": "list"])])[0].result
        for s in list["schedules"].asArray ?? [] {
            #expect((s["run_count"].asInt ?? 0) >= 1, "\(s)")
        }
    }

    @Test func nextRunIsPersistedBeforeTheFire() async throws {
        // The probe reads schedules.yaml the moment it is hit: the store must
        // already hold the bumped run_count/next_run (persist-BEFORE-fire).
        let fm = FileManager.default
        let rel = "fantastic-schedpersist-\(UUID().uuidString)"
        let dir = URL(fileURLWithPath: fm.currentDirectoryPath).appendingPathComponent(rel)
        try fm.createDirectory(at: dir, withIntermediateDirectories: true)
        defer { try? fm.removeItem(at: dir) }
        let capture = dir.appendingPathComponent("agents/sched/schedules.yaml")
        // SCHEDULED WHILE THE SWEEP IS OFF, then released: `create_agent` boots
        // the scheduler, so its loop was reading `schedules.yaml` while
        // `schedule` wrote it and the stale read landed on the cache after the
        // write — the schedule was lost and the probe was never hit under the
        // full parallel run. `paused` is checked before the loop reads.
        let (kernel, probe) = await makeProbedScheduler(
            dir: dir, rel: rel, probes: ["p_cap"], capture: capture, paused: true)
        let r = await schedule(kernel, "p_cap")
        let sid = try #require(r["schedule_id"].asString)
        _ = await kernel.send([Item("sched", ["type": "boot"])])[0].result
        let resumed = await kernel.send([Item("kernel", [
                "type": "update_agent", "id": "sched", "paused": false,
            ])])[0].result
        #expect(resumed["error"].isNull, "\(resumed)")
        let hit = await waitFor({ probe.hit("p_cap") != nil }, seconds: 3.0)
        _ = await kernel.send([Item("kernel", ["type": "delete_agent", "id": "sched"])])[0].result
        #expect(hit, "probe must be hit")
        let seen = try #require(probe.hit("p_cap"))
        let doc = try #require(seen.doc, "store had no schedules.yaml at fire time")
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
        // THE SWEEP IS OFF for this one: `create_agent` boots the scheduler, so
        // its loop is already reading `schedules.yaml` while `schedule` below
        // writes it, and the loop's in-flight read landed on the cache AFTER
        // the write — `tick_now` then answered `schedule '…' not found` under
        // the full parallel run. The test is about `tick_now` and its timeout
        // bound, never about the loop; `paused` is checked before the loop
        // reads, so nothing races. `tick_now` fires regardless of `paused`.
        let (kernel, _) = await makeProbedScheduler(
            dir: dir, rel: rel, probes: ["p_hang"], paused: true)
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
        _ = await kernel.send([Item("kernel", ["type": "delete_agent", "id": "sched"])])[0].result
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
        _ = await kernel.send([Item("kernel", ["type": "delete_agent", "id": "sched"])])[0].result
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
        _ = await kernel.send([Item("kernel", ["type": "delete_agent", "id": "sched"])])[0].result
    }

    @Test func aNonNumericTickSecIsRefusedAtWriteTime() async {
        // #136 — `tick_sec: "0.5"` was silently defaulted to 1.0. The record is
        // judged where the mistake is made.
        let kernel = makeKernelWithAll()
        let r = await kernel.send([Item("kernel_state", [
                "type": "create_agent", "parent_id": "kernel_state",
                "handler_module": "scheduler.tools", "id": "sch_ticks", "tick_sec": "0.5",
            ])])[0].result
        #expect(r["reason"].asString == "invalid_meta", "\(r.serialize())")
        #expect((r["error"].asString ?? "").contains("tick_sec"), "\(r.serialize())")
        #expect(kernel.agent(AgentId("sch_ticks")) == nil)
    }

    @Test func kernelBridgeForwardInMemory() async {
        let kernel1 = makeKernelWithAll()
        let kernel2 = makeKernelWithAll()
        // Wire bridge from kernel1 → kernel2: python's `memory` seam injects each
        // half of a pair, then BOOTS — kernel2's leg serves the forward.
        await wireMemoryPair(kernel1, kernel2)
        let reply = await kernel1.send([Item("br", [
                "type": "forward",
                "target": "kernel_state",
                "payload": ["type": "list_agents"] as JSON,
            ])])[0].result
        // kernel2's list_agents returns "agents" array including core.
        let names = (reply["agents"].asArray ?? []).map { $0["id"].asString ?? "" }
        #expect(names.contains("kernel_state"))
    }

    @Test func kernelBridgeBinaryForwardStreamsRawBytes() async throws {
        // Cross-kernel STREAMING — forward a write_stream/read_stream over the
        // (in-memory) bridge to a remote file_bridge; raw bytes both ways.
        let kernel1 = makeKernelWithAll()
        let kernel2 = makeKernelWithAll()
        await wireMemoryPair(kernel1, kernel2)
        // kernel2: an OPEN file_bridge in a cwd-relative dir, under the serving
        // leg, accepting that leg (what it serves is the leg's own call).
        let fm = FileManager.default
        let rel = "fantastic-brstream-\(UUID().uuidString)"
        let dir = URL(fileURLWithPath: fm.currentDirectoryPath).appendingPathComponent(rel)
        try fm.createDirectory(at: dir, withIntermediateDirectories: true)
        defer { try? fm.removeItem(at: dir) }
        _ = await kernel2.send([Item("kernel_state", [
                "type": "create_agent", "parent_id": "br_peer", "handler_module": "file_bridge.tools", "allow": ["."], "id": "fs",
                "root": .string(rel), "ingress_rule": "allow_all_ingress",
                "callable_by": ["br_peer": "*"],
            ])])[0].result
        let payload = Data([0x00, 0xFF, 0xCA, 0xFE, 0xBA, 0xBE, 0x10, 0x80])
        // forward write_stream (binary) → kernel2.fs
        let w = await kernel1.send([
            Item(
                "br",
                .object([
                    "type": .string("forward"), "target": .string("fs"),
                    "payload": .object([
                        "type": .string("write_stream"), "path": .string("blob.bin"),
                        "truncate": .bool(true),
                    ]),
                ]), binary: Blob(parts: ["payload", "bytes"], payload))
        ])[0].result
        #expect(w["written"].asInt == Int64(payload.count), "\(w)")
        // forward read_stream (binary) → bytes back over the bridge
        let body = await kernel1.send([
            Item(
                "br",
                .object([
                    "type": .string("forward"), "target": .string("fs"),
                    "payload": .object([
                        "type": .string("read_stream"), "path": .string("blob.bin"),
                    ]),
                ]), rawReply: true)
        ])[0].body
        #expect(
            body == Blob("bytes", payload),
            "raw bytes must round-trip cross-kernel over the bridge, at the reply's `bytes`")
    }

    @Test func wsBinaryForwardStreamsRawBytesOverWire() async throws {
        // The WIRE half of cross-kernel streaming — a bridge's
        // `WebSocketTransport.binaryForward` ships a codec binary frame over a
        // real loopback WS to a remote `web_ws`, which decodes it, gates, and
        // sends one binary item to a `file_bridge`. Raw bytes both ways,
        // never base64. (The in-memory variant is
        // `kernelBridgeBinaryForwardStreamsRawBytes`.)
        let kernel = try await startWebKernel()
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
                // of web) — no baseline token reaches it.
                "can_call": .object(["fs": .string("*")]),
            ]))])[0].result
        let wsId = rec["id"].asString ?? "web_ws"
        wire(kernel, "web", wsId)
        _ = await kernel.send([Item(AgentId("web"), .object(["type": .string("mount"), "child_id": .string(wsId)]))])[0].result
        defer {
            Task {
                _ = await kernel.send([Item(AgentId("web"), .object(["type": .string("stop")]))])[0].result
            }
        }
        // An OPEN file_bridge in a cwd-relative dir on the SAME kernel.
        let fm = FileManager.default
        let rel = "fantastic-wsbrstream-\(UUID().uuidString)"
        let dir = URL(fileURLWithPath: fm.currentDirectoryPath)
            .appendingPathComponent(rel)
        try fm.createDirectory(at: dir, withIntermediateDirectories: true)
        defer { try? fm.removeItem(at: dir) }
        _ = await kernel.send([Item(AgentId("kernel_state"), .object([
                "type": .string("create_agent"),
                "parent_id": .string("kernel_state"),
                "handler_module": .string("file_bridge.tools"), "allow": ["."],
                "id": .string("fs"), "root": .string(rel),
                "ingress_rule": .string("allow_all_ingress"),
                // …and the INGRESS half, on the target.
                "callable_by": .object([
                    "$self": .string("*"), "$parent": .string("*"),
                    "$subtree": .string("*"), "web_ws": .string("*"),
                ]),
            ]))])[0].result

        // A ws_bridge leg dialing this kernel's own web_ws over the loopback.
        let port = kernel.httpPort()
        let brBundle = KernelBridgeBundle(family: .ws)
        kernel.bundles.register("ws_bridge.tools", brBundle)
        _ = await kernel.send([Item(AgentId("kernel_state"), .object([
                "type": .string("create_agent"), "parent_id": .string("kernel_state"),
                "handler_module": .string("ws_bridge.tools"), "id": .string("wbr"),
                "transport": .string("ws"), "host": .string("127.0.0.1"),
                "port": .integer(Int64(port)), "peer_id": .string("kernel_state"),
            ]))])[0].result
        // `create_agent` already booted it through the kernel's fanout, so this
        // second boot is python's idempotent `{already:true, transport}`.
        let booted = await kernel.send([Item(AgentId("wbr"), .object(["type": .string("boot")]))])[0].result
        #expect(booted == .object(["already": .bool(true), "transport": .string("ws")]), "\(booted.serialize())")

        let payload = Data([0x00, 0xFF, 0xCA, 0xFE, 0xBA, 0xBE, 0x10, 0x80])
        // write_stream (binary) over the wire → kernel.fs
        let w = await kernel.send([
            Item(
                AgentId("wbr"),
                .object([
                    "type": .string("forward"), "target": .string("fs"),
                    "payload": .object([
                        "type": .string("write_stream"), "path": .string("blob.bin"),
                        "truncate": .bool(true),
                    ]),
                ]), binary: Blob(parts: ["payload", "bytes"], payload))
        ])[0].result
        #expect(w["written"].asInt == Int64(payload.count), "\(w.serialize())")
        // read_stream (binary) over the wire → raw bytes back
        let body = await kernel.send([
            Item(
                AgentId("wbr"),
                .object([
                    "type": .string("forward"), "target": .string("fs"),
                    "payload": .object([
                        "type": .string("read_stream"), "path": .string("blob.bin"),
                    ]),
                ]), rawReply: true)
        ])[0].body
        #expect(
            body == Blob("bytes", payload),
            "raw bytes must round-trip over the WS wire, at the reply's `bytes`")
    }

    @Test func kernelBridgeWatchRemoteReEmitsOnLocalInbox() async throws {
        // A's bridge subscribes to a remote agent's emits via watch_remote: the
        // `watch` frame crosses the memory pair, and the remote's `event` frame
        // for that agent re-emits on A's bridge inbox, `src` added (python
        // `test_event_frame_re_emits_on_bridge_inbox_only_when_subscribed`).
        let kernelA = makeKernelWithAll()
        let bridge = KernelBridgeBundle()
        kernelA.bundles.register("ws_bridge.tools", bridge)
        _ = await kernelA.send([Item("kernel_state", ["type": "create_agent", "parent_id": "kernel_state", "handler_module": "ws_bridge.tools", "id": "br", "transport": "memory"])])[0].result
        let (mine, peer) = MemoryTransport.pair()
        bridge.injectTransport(agentId: "br", transport: mine)
        _ = await kernelA.send([Item("br", ["type": "boot"])])[0].result

        // Subscribe via the bridge: A.br.watch_remote(target=core)
        let w = await kernelA.send([Item("br", ["type": "watch_remote", "target": "kernel_state"])])[0].result
        #expect(w["ok"].asBool == true)
        #expect(try await peer.recv().frame == ["type": "watch", "src": "kernel_state"])

        // Synthetic local watcher of the bridge agent's inbox. Every
        // re-emitted event lands here.
        let watcherId: AgentId = "test_local_watcher"
        // THE WATCHER REGISTERS FIRST — `watch` mirrors into an inbox, it does
        // not create one on the caller's behalf.
        let inbox = kernelA.ensureInbox(watcherId)
        kernelA.watch(src: "br", watcher: watcherId)

        // The remote's event for the watched agent.
        try await peer.send(
            ["type": "event", "src": "kernel_state", "payload": ["type": "token", "text": "hi"]])

        // Wait for the event with a short timeout.
        let event = await withTaskGroup(of: JSON?.self) { group in
            group.addTask {
                for await ev in inbox {
                    if ev.payload["type"].asString == "token" { return ev.payload }
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
        #expect(event?["src"].asString == "kernel_state")

        _ = await kernelA.send([Item("br", ["type": "unwatch_remote", "target": "kernel_state"])])[0].result
    }

    /// A11.5 — python's ws descriptor is `{kind:"websocket", path}` with NO
    /// method (the host defaults it); swift declared `method: "GET"`.
    @Test func webWSGetRoutesShape() async {
        let kernel = makeKernelWithAll()
        _ = await kernel.send([Item("kernel_state", ["type": "create_agent", "parent_id": "kernel_state", "handler_module": "web_ws.tools", "id": "ws"])])[0].result
        let r = await kernel.send([Item("ws", ["type": "get_routes"])])[0].result
        let routes = r["routes"].asArray ?? []
        #expect(routes.count == 1)
        #expect(routes.first?["kind"].asString == "websocket")
        #expect(routes.first?["path"].asString == "/{host_id}/ws")
        #expect(routes.first?["method"].isNull == true, "\(r)")
        // `docs` publishes python's FIRST docstring line, `No args.` included.
        let refl = await kernel.send([Item("kernel", ["type": "docs", "of": "ws", "tier": "detailed"])])[0].result
        #expect(
            refl["verbs"]["get_routes"].asString
                == "Declares this surface's HTTP/WS route mounts. No args. Returns", "\(refl)")
    }

    /// A11.5 — the REST paths capture `{target_id}`, python's parameter name.
    @Test func webRestGetRoutesNamespacedBySelf() async {
        let kernel = makeKernelWithAll()
        _ = await kernel.send([Item("kernel_state", ["type": "create_agent", "parent_id": "kernel_state", "handler_module": "web_rest.tools", "id": "rest"])])[0].result
        let r = await kernel.send([Item("rest", ["type": "get_routes"])])[0].result
        let paths = (r["routes"].asArray ?? []).compactMap { $0["path"].asString }
        #expect(paths == ["/rest/{target_id}", "/rest/_reflect", "/rest/_reflect/{target_id}"])
        let methods = (r["routes"].asArray ?? []).compactMap { $0["method"].asString }
        #expect(methods == ["POST", "GET", "GET"])
    }

    /// A11.28 — `handle_route` is NOT an agent verb: python mounts a callable,
    /// so a served request is never a send on the leg (swift's extra hop held a
    /// second, parent task row that `shutdown_agents` then cancelled).
    @Test func webRestHandleRouteIsNotAVerb() async {
        let kernel = makeKernelWithAll()
        _ = await kernel.send([Item("kernel_state", [
                "type": "create_agent", "parent_id": "kernel_state", "handler_module": "web_rest.tools", "id": "rest",
                "ingress_rule": "allow_all_ingress",
            ])])[0].result
        let reply = await kernel.send([Item("rest", [
                "type": "handle_route",
                "method": "POST",
                "params": ["target_id": "kernel_state"] as JSON,
                "body": "{\"type\":\"list_agents\"}",
            ])])[0].result
        #expect(reply["reason"].asString == "unhandled", "\(reply)")
        #expect(reply["error"].asString == "web_rest: unknown type 'handle_route'", "\(reply)")
        #expect(reply["verb"].asString == "handle_route", "\(reply)")
    }
}

/// A probe target for the scheduler tests: `hit` records when it ran (and the
/// scheduler's persisted `schedules.yaml` at that instant when `capture` is
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
                doc = try? YamlIO.load(raw)
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

// ─── A5 python-parity twins ─────────────────────────────────────────
//
// One test per plan item, each pinning the PYTHON text or behaviour named there
// (`PB/scheduler/src/scheduler/tools.py`). Python is canonical; where a line here
// disagrees with it, this file is wrong.

/// One dispatch, spelled once.
private func ask(_ kernel: Kernel, _ target: String, _ payload: JSON) async -> JSON {
    await kernel.send([Item(AgentId(target), payload)])[0].result
}

/// `create_agent` for a scheduler on the root, with optional extra meta.
@discardableResult
private func newScheduler(
    _ kernel: Kernel, _ id: String, _ meta: [String: JSON] = [:]
) async -> JSON {
    var p: JSON = [
        "type": "create_agent", "parent_id": "kernel_state",
        "handler_module": "scheduler.tools", "id": .string(id),
    ]
    for (k, v) in meta { p[k] = v }
    return await ask(kernel, "kernel_state", p)
}

/// A store-backed kernel with a `sched` that may persist through it. Returns the
/// kernel and the store's on-disk directory.
private func schedulerOnAStore(_ tag: String) async throws -> (Kernel, URL) {
    let fm = FileManager.default
    let rel = "fantastic-\(tag)-\(UUID().uuidString)"
    let dir = URL(fileURLWithPath: fm.currentDirectoryPath).appendingPathComponent(rel)
    try fm.createDirectory(at: dir, withIntermediateDirectories: true)
    let kernel = makeKernelWithAll(rootPath: dir)
    var store: JSON = [
        "type": "create_agent", "parent_id": "kernel_state",
        "handler_module": "file_bridge.tools", "allow": ["."], "id": "store",
        "root": .string(rel), "ingress_rule": "allow_all_ingress",
    ]
    store["callable_by"] = ["sched": "*"]
    _ = await ask(kernel, "kernel_state", store)
    await newScheduler(kernel, "sched", ["can_call": ["store": "*"] as JSON])
    // a schedule fires AS the scheduler: its own edge to the root
    wire(kernel, "sched", "kernel_state")
    return (kernel, dir)
}

/// Schedule `payload` on `target` and return the whole reply.
private func addSchedule(
    _ kernel: Kernel, target: JSON, payload: JSON, interval: Int64 = 3600,
    timeout: JSON? = nil
) async -> JSON {
    var p: JSON = [
        "type": "schedule", "target": target, "interval_seconds": .integer(interval),
        "payload": payload,
    ]
    if let timeout { p["timeout"] = timeout }
    return await ask(kernel, "sched", p)
}

/// python's `readme.md`, for the byte-for-byte pin.
private func pythonSchedulerReadme() throws -> String {
    let url = URL(fileURLWithPath: #filePath)
        .deletingLastPathComponent()  // FantasticBundlesTests
        .deletingLastPathComponent()  // Tests
        .deletingLastPathComponent()  // swift
        .deletingLastPathComponent()  // lib
        .appendingPathComponent("python/bundled_agents/scheduler/src/scheduler/readme.md")
    return try String(contentsOf: url, encoding: .utf8)
}

@Suite("scheduler python parity")
struct SchedulerParityTests {

    /// A5.1 — the `schedule_fired` blurb is python's, verbatim: two em-dashes and
    /// the clause that says the payload send IS the tick.
    @Test func schedulerDocsEmitsIsPythonVerbatim() async throws {
        let kernel = makeKernelWithAll()
        await newScheduler(kernel, "s_emits")
        #expect(await ask(kernel, "s_emits", ["type": "reflect"]).asObject?["emits"] == nil)
        let r = await ask(kernel, "kernel", ["type": "docs", "of": "s_emits", "tier": "detailed"])
        #expect(
            r["emits"]["schedule_fired"].asString
                == "{type:'schedule_fired', schedule_id, scheduler_id, target, payload, result, error, ts, duration_ms} — emitted on the scheduler's OWN inbox after every fire (never the target's — the payload send is the tick)",
            "\(r["emits"].serialize())")
    }

    /// A5.2 — the readme IS python's `readme.md`, byte for byte, not a swift
    /// paraphrase.
    @Test func schedulerReadmeIsPythonVerbatim() throws {
        #expect(SchedulerBundle().readme == (try pythonSchedulerReadme()))
    }

    /// A5.3 + A5.13 — `shutdown` is not a verb here: python's VERBS table has no
    /// such key, so it is the unknown-type error, spelled with `{t!r}`.
    @Test func schedulerShutdownIsAnUnknownType() async throws {
        let kernel = makeKernelWithAll()
        await newScheduler(kernel, "s_sd")
        let r = await ask(kernel, "s_sd", ["type": "shutdown"])
        #expect(r["error"].asString == "scheduler: unknown type 'shutdown'", "\(r.serialize())")
        let refl = await ask(kernel, "s_sd", ["type": "reflect"])
        #expect(refl["verbs"].asObject?["shutdown"] == nil, "\(refl["verbs"].serialize())")
        let n = await ask(kernel, "s_sd", ["type": "wat"])
        #expect(n["error"].asString == "scheduler: unknown type 'wat'", "\(n.serialize())")
    }

    /// A5.4 — `boot` ALWAYS replies `{running:true}` and nothing else, and it
    /// RESTARTS a loop whose task is gone (a cancelled tick loop left its id in
    /// the table, so the scheduler claimed to run and never ticked again).
    @Test func schedulerBootAfterCancelRestartsTheTickLoop() async throws {
        let kernel = makeKernelWithAll()
        await newScheduler(kernel, "s_boot")
        let first = await ask(kernel, "s_boot", ["type": "boot"])
        #expect(first == .object(["running": .bool(true)]), "\(first.serialize())")
        let second = await ask(kernel, "s_boot", ["type": "boot"])
        #expect(second == .object(["running": .bool(true)]), "\(second.serialize())")

        // Cancel the loop through the task table, then boot again.
        let ps = await ask(kernel, "kernel", ["type": "ps"])
        let rows = ps["tasks"].asArray ?? []
        let row = rows.first {
            $0["agent"].asString == "s_boot" && $0["verb"].asString == "tick_loop"
        }
        let tid = try #require(row?["id"].asInt, "no tick_loop row: \(ps.serialize())")
        _ = await ask(kernel, "kernel", ["type": "cancel", "task_id": .integer(tid)])
        let dead = await ask(kernel, "s_boot", ["type": "reflect"])
        #expect(dead["running"].asBool == false, "the loop must be gone: \(dead.serialize())")

        let again = await ask(kernel, "s_boot", ["type": "boot"])
        #expect(again == .object(["running": .bool(true)]), "\(again.serialize())")
        let back = await ask(kernel, "s_boot", ["type": "reflect"])
        #expect(
            back["running"].asBool == true,
            "boot must restart a dead loop: \(back.serialize())")
    }

    /// A5.5 — the VALIDATION ORDER is observable. python checks the payload's
    /// shape, then `interval_seconds`, then `target`, then `payload.type`, then
    /// `timeout`; a call with two faults must name the first of those.
    @Test func schedulerValidationOrderMatchesPython() async throws {
        let (kernel, dir) = try await schedulerOnAStore("schedorder")
        defer { try? FileManager.default.removeItem(at: dir) }
        // interval BEFORE target: both wrong ⇒ the interval is named.
        let a = await ask(
            kernel, "sched",
            [
                "type": "schedule", "target": "", "interval_seconds": "60",
                "payload": ["type": "hit"] as JSON,
            ])
        #expect(a["reason"].asString == "bad_arg", "\(a.serialize())")
        #expect(a["arg"].asString == "interval_seconds", "\(a.serialize())")
        // payload SHAPE before the interval.
        let b = await ask(
            kernel, "sched",
            [
                "type": "schedule", "target": "kernel_state", "interval_seconds": "60",
                "payload": .array([.integer(1)]),
            ])
        #expect(b["arg"].asString == "payload", "\(b.serialize())")
        // target BEFORE payload.type.
        let c = await addSchedule(kernel, target: "", payload: ["note": "no type"])
        #expect(c["error"].asString == "schedule: target required", "\(c.serialize())")
        // …and payload.type BEFORE timeout.
        let d = await addSchedule(
            kernel, target: "kernel_state", payload: ["note": "no type"], timeout: .integer(0))
        #expect(d["error"].asString == "schedule: payload.type required", "\(d.serialize())")
        // A truthy non-string target is STORED VERBATIM, python-style.
        let e = await addSchedule(kernel, target: .integer(7), payload: ["type": "hit"])
        #expect(e["schedule"]["target"] == .integer(7), "\(e.serialize())")
    }

    /// A5.6 — `timeout` is stored AS SENT: an int `5` stays an int, in the reply
    /// and in the persisted bytes.
    @Test func anIntTimeoutStaysAnInt() async throws {
        let (kernel, dir) = try await schedulerOnAStore("schedto")
        defer { try? FileManager.default.removeItem(at: dir) }
        let r = await addSchedule(
            kernel, target: "kernel_state", payload: ["type": "list_agents"], timeout: .integer(5))
        #expect(r["schedule"]["timeout"] == .integer(5), "\(r.serialize())")
        let raw = try String(
            contentsOf: dir.appendingPathComponent("agents/sched/schedules.yaml"),
            encoding: .utf8)
        #expect(raw.contains("  timeout: 5\n"), "widened on disk: \(raw)")
    }

    /// A5.7 — every `schedule_id` read is python TRUTHINESS, and the value is
    /// echoed RAW.
    @Test func emptyScheduleIdIsAbsentLikePython() async throws {
        let (kernel, dir) = try await schedulerOnAStore("schedsid")
        defer { try? FileManager.default.removeItem(at: dir) }
        for falsy in [JSON.string(""), .null, .integer(0)] {
            let u = await ask(kernel, "sched", ["type": "unschedule", "schedule_id": falsy])
            #expect(u["error"].asString == "unschedule: schedule_id required", "\(u.serialize())")
            let t = await ask(kernel, "sched", ["type": "tick_now", "schedule_id": falsy])
            #expect(t["error"].asString == "tick_now: schedule_id required", "\(t.serialize())")
            // pause with a FALSY id pauses the WHOLE scheduler.
            let p = await ask(kernel, "sched", ["type": "pause", "schedule_id": falsy])
            #expect(p["scheduler_id"].asString == "sched", "\(p.serialize())")
        }
        // A truthy id nobody holds is echoed raw, in python's repr.
        let miss = await ask(kernel, "sched", ["type": "tick_now", "schedule_id": .integer(9)])
        #expect(miss["error"].asString == "schedule 9 not found", "\(miss.serialize())")
        let un = await ask(kernel, "sched", ["type": "unschedule", "schedule_id": .integer(9)])
        #expect(un["schedule_id"] == .integer(9), "\(un.serialize())")
        #expect(un["removed"].asBool == false, "\(un.serialize())")
    }

    /// A5.13 — `schedule {sid!r} not found` quotes a string id.
    @Test func schedulerRefusalTextUsesPythonRepr() async throws {
        let (kernel, dir) = try await schedulerOnAStore("schedrepr")
        defer { try? FileManager.default.removeItem(at: dir) }
        let r = await ask(kernel, "sched", ["type": "tick_now", "schedule_id": "sch_zzz"])
        #expect(r["error"].asString == "schedule 'sch_zzz' not found", "\(r.serialize())")
    }

    /// A5.8 — the fire outcome: COMPLETED puts the target's reply in `result` and
    /// leaves `error` null; anything else makes `'<status>: ' + str(detail)`, where
    /// the detail is `res["error"]` for a dict body and the body itself otherwise
    /// (so a cancelled fire with no detail reads `cancelled: None`).
    ///
    /// The scheduler used to hoist an `error` key out of a COMPLETED reply, which
    /// reported a failed fire for a verb that had answered.
    @Test func cancelledFireErrorReadsNone() async throws {
        let (kernel, dir) = try await schedulerOnAStore("schedout")
        defer { try? FileManager.default.removeItem(at: dir) }
        func fire(_ target: String, _ payload: JSON) async -> JSON {
            let r = await addSchedule(kernel, target: .string(target), payload: payload)
            let sid = r["schedule_id"].asString ?? ""
            _ = await ask(kernel, "sched", ["type": "tick_now", "schedule_id": .string(sid)])
            let h = await ask(
                kernel, "sched", ["type": "history", "schedule_id": .string(sid)])
            return h["history"][0]
        }
        // COMPLETED: the reply is the result, and there is NO error.
        let ok = await fire("kernel_state", ["type": "list_agents"])
        #expect(ok["error"].isNull, "a completed fire has no error: \(ok.serialize())")
        #expect(!ok["result"]["agents"].isNull, "\(ok.serialize())")

        // NOT COMPLETED: `<status>: <str(detail)>`, and `result` stays null. The
        // detail here is the body's `error`; a body that is not a dict (a cancelled
        // fire's `None`) renders through the same `str()`.
        let bad = await fire("store", ["type": "list", "path": "nope_dir"])
        #expect(
            bad["error"].asString == "errored: path 'nope_dir' does not exist",
            "\(bad.serialize())")
        #expect(bad["result"].isNull, "\(bad.serialize())")
    }

    /// A5.10 — the sidecars are python's `messages_yaml.dump`: `schedules.yaml`
    /// is a block-style YAML sequence, `schedule_events.yaml` a YAML stream of
    /// one `---` document per fire, BMP text verbatim (`allow_unicode`).
    @Test func sidecarsUsePythonYamlLayout() async throws {
        let (kernel, dir) = try await schedulerOnAStore("schedjson")
        defer { try? FileManager.default.removeItem(at: dir) }
        let r = await addSchedule(
            kernel, target: "kernel_state", payload: ["type": "list_agents", "note": "héllo"])
        let sid = try #require(r["schedule_id"].asString)
        let sched = try String(
            contentsOf: dir.appendingPathComponent("agents/sched/schedules.yaml"),
            encoding: .utf8)
        #expect(sched.hasPrefix("- id: \(sid)\n  target: kernel_state\n"), "block sequence: \(sched.prefix(60))")
        #expect(sched.contains("    note: héllo\n"), "allow_unicode: \(sched)")

        _ = await ask(kernel, "sched", ["type": "tick_now", "schedule_id": .string(sid)])
        let hist = try String(
            contentsOf: dir.appendingPathComponent("agents/sched/schedule_events.yaml"),
            encoding: .utf8)
        #expect(
            hist.hasPrefix("---\ntype: schedule_fired\nschedule_id: \(sid)\n"),
            "one `---` document per fire: \(hist.prefix(120))")
        #expect(hist.contains("note: héllo\n"), "allow_unicode on the event: \(hist)")
        #expect(hist.hasSuffix("\n"), "every document newline-terminated")
        #expect(try YamlIO.loadAll(hist).count == 1, "\(hist)")
    }

    /// A5.11 — `paused` is read with python TRUTHINESS, on the record and on each
    /// schedule; reflect still reports a BOOL.
    @Test func pausedMetaUsesPythonTruthiness() async throws {
        let kernel = makeKernelWithAll()
        await newScheduler(kernel, "s_paused", ["paused": "yes"])
        let r = await ask(kernel, "s_paused", ["type": "reflect"])
        #expect(
            r["paused"] == .bool(true),
            "a truthy string is paused, as a BOOL: \(r.serialize())")

        _ = await ask(
            kernel, "kernel_state", ["type": "update_agent", "id": "s_paused", "paused": .integer(0)])
        let z = await ask(kernel, "s_paused", ["type": "reflect"])
        #expect(z["paused"] == .bool(false), "0 is not paused: \(z.serialize())")
    }

    /// A5.12 — NO 0.1 s floor. `validateMeta` already refuses everything the read
    /// cannot use, so a clamp could only contradict a legal record.
    @Test func tickSecBelowATenthIsNotClamped() async throws {
        let kernel = makeKernelWithAll()
        await newScheduler(kernel, "s_fast", ["tick_sec": .double(0.02)])
        let r = await ask(kernel, "s_fast", ["type": "reflect"])
        #expect(r["tick_sec"].asDouble == 0.02, "\(r.serialize())")
        // An absent one is the documented default, not a clamp.
        await newScheduler(kernel, "s_dflt")
        let d = await ask(kernel, "s_dflt", ["type": "reflect"])
        #expect(d["tick_sec"].asDouble == 1.0, "\(d.serialize())")
    }

    /// A5.16 — pause/resume put the FLAG FIRST, python's dict order.
    @Test func schedulerPauseReplyKeyOrderMatchesPython() async throws {
        let (kernel, dir) = try await schedulerOnAStore("schedkeys")
        defer { try? FileManager.default.removeItem(at: dir) }
        let r = await addSchedule(kernel, target: "kernel_state", payload: ["type": "list_agents"])
        let sid = try #require(r["schedule_id"].asString)
        let p = await ask(kernel, "sched", ["type": "pause", "schedule_id": .string(sid)])
        #expect(
            Array(try #require(p.asObject).keys) == ["paused", "schedule_id"],
            "\(p.serialize())")
        let rs = await ask(kernel, "sched", ["type": "resume", "schedule_id": .string(sid)])
        #expect(
            Array(try #require(rs.asObject).keys) == ["resumed", "schedule_id"],
            "\(rs.serialize())")
        let whole = await ask(kernel, "sched", ["type": "pause"])
        #expect(
            Array(try #require(whole.asObject).keys) == ["paused", "scheduler_id"],
            "\(whole.serialize())")
    }

    /// A5.9 — the RAM cache is updated BEFORE the persist, and the two directions
    /// differ: `pause` KEEPS its flip when the write fails (the next tick must
    /// already honour it), while `tick_now` ROLLS its bump back (a run nothing
    /// recorded must not show up in `list`).
    ///
    /// Acceptance-deferred until C13: the persist-failure TEXT wraps the loader's
    /// own refusal, which C13 rewrites. What is pinned here is the RAM half.
    @Test func failedTickPersistDoesNotRefireNextTick() async throws {
        let (kernel, dir) = try await schedulerOnAStore("schedcache")
        defer { try? FileManager.default.removeItem(at: dir) }
        let r = await addSchedule(kernel, target: "kernel_state", payload: ["type": "list_agents"])
        let sid = try #require(r["schedule_id"].asString)
        // BREAK PERSISTENCE: with the store gone the loader has nothing to write to.
        _ = await ask(kernel, "kernel", ["type": "delete_agent", "id": "store"])

        let paused = await ask(kernel, "sched", ["type": "pause", "schedule_id": .string(sid)])
        #expect(
            (paused["error"].asString ?? "").hasPrefix("scheduler.pause:"),
            "the failed write must be surfaced: \(paused.serialize())")
        let afterPause = await ask(kernel, "sched", ["type": "list"])
        #expect(
            afterPause["schedules"][0]["paused"].asBool == true,
            "a failed write KEEPS the flip: \(afterPause.serialize())")

        let ticked = await ask(kernel, "sched", ["type": "tick_now", "schedule_id": .string(sid)])
        #expect(
            (ticked["error"].asString ?? "").hasPrefix("scheduler.tick_now:"),
            "\(ticked.serialize())")
        let afterTick = await ask(kernel, "sched", ["type": "list"])
        #expect(
            afterTick["schedules"][0]["run_count"].asInt == 0,
            "a failed tick_now ROLLS the bump back: \(afterTick.serialize())")
    }

    /// A5.14 — `delete_agent` evicts the schedule cache (python `on_delete`,
    /// 2026-09-20): a scheduler recreated under the same id starts empty instead
    /// of inheriting its predecessor's schedules.
    @Test func recreatedSchedulerDropsTheProcessCache() async throws {
        let (kernel, dir) = try await schedulerOnAStore("schedrecreate")
        defer { try? FileManager.default.removeItem(at: dir) }
        _ = await addSchedule(kernel, target: "kernel_state", payload: ["type": "list_agents"])
        _ = await ask(kernel, "kernel", ["type": "delete_agent", "id": "sched"])
        // The sidecar is gone from disk, so only the PROCESS cache could answer
        // — and it went with the deleted agent.
        try FileManager.default.removeItem(at: dir.appendingPathComponent("agents/sched/schedules.yaml"))
        await newScheduler(kernel, "sched")
        let list = await ask(kernel, "sched", ["type": "list"])
        #expect((list["schedules"].asArray ?? []).count == 0, "\(list.serialize())")
    }
}

// ── A15.2 — the scheduler names its persistence failures on stderr ──

@Suite("scheduler stderr lines")
struct SchedulerStderrTests {

    /// Python `scheduler/tools.py:236-239,286-289`: a tick that cannot persist
    /// prints `scheduler <sid>: persist failed: <error>` and a fire that cannot
    /// append prints `scheduler <sid>: history persist failed: <error>`. Swift
    /// discarded both results, so a scheduler whose store went away ticked on in
    /// silence and lost every schedule on the next boot with nothing anywhere
    /// saying why.
    @Test func schedulerPersistFailurePrintsPythonLine() async throws {
        let fm = FileManager.default
        let rel = "fantastic-a152-\(UUID().uuidString)"
        let dir = URL(fileURLWithPath: fm.currentDirectoryPath).appendingPathComponent(rel)
        try fm.createDirectory(at: dir, withIntermediateDirectories: true)
        defer { try? fm.removeItem(at: dir) }
        let kernel = makeKernelWithAll(rootPath: dir)

        _ = await ask(
            kernel, "kernel_state",
            [
                "type": "create_agent", "parent_id": "kernel_state",
                "handler_module": "file_bridge.tools", "allow": ["."], "id": "store",
                "root": .string(rel), "ingress_rule": "allow_all_ingress",
                "callable_by": ["sched": "*"] as JSON,
            ])
        _ = await ask(
            kernel, "kernel_state",
            [
                "type": "create_agent", "parent_id": "kernel_state",
                "handler_module": "scheduler.tools", "id": "sched",
                "tick_sec": .double(0.25),
                "can_call": ["store": "*", "kernel_state": "*"] as JSON,
            ])
        _ = await ask(kernel, "sched", ["type": "boot"])
        let added = await ask(
            kernel, "sched",
            [
                "type": "schedule", "target": "kernel_state", "interval_seconds": .integer(1),
                "payload": ["type": "list_agents"] as JSON,
            ])
        #expect(added["error"].asString == nil, "\(added.serialize())")

        // Pull the store out from under the running tick loop — every write the
        // next sweep attempts now fails at the loader.
        _ = await ask(kernel, "kernel", ["type": "delete_agent", "id": "store"])

        // One sweep window: tick_sec 0.25, the schedule due at +1s. Kept as
        // short as the 1 s minimum interval allows — a live tick loop is load on
        // every test running beside it.
        let lines = await RuntimeLog.capture {
            try? await Task.sleep(nanoseconds: 1_600_000_000)
        }
        _ = await ask(kernel, "kernel", ["type": "delete_agent", "id": "sched"])
        let persist = lines.filter { $0.hasPrefix("scheduler sched: persist failed: ") }
        let history = lines.filter { $0.hasPrefix("scheduler sched: history persist failed: ") }

        #expect(!persist.isEmpty, "no persist-failed line")
        #expect(!history.isEmpty, "no history-persist-failed line")
        // The tail is the loader's own error text, and it is NEVER empty — an
        // empty tail would mean the error dict lost its `error` key.
        for line in persist + history {
            #expect(line.hasSuffix("\n"))
            #expect(line.split(separator: ": ").count >= 3, "\(line)")
            #expect(!line.hasSuffix("failed: \n"), "\(line)")
        }
    }
}
