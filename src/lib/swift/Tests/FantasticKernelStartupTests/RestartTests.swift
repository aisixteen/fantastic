// RESTART THROUGH THE REAL STORE — a disk-backed kernel (`startKernel`), the
// real `file_bridge` provider discovered at `.fantastic` (records only: the
// kernel writes NO message log), a `ram_store` memory store named by the root,
// and a FRESH kernel on the same workdir. The root record carries the guid AND
// the id counter's reserved bound, so a restart never repeats an id; a
// `ram_store` dies with the process, so the harness `dump`s it before the stop
// and `inject`s it after the next start.
//
// Twin of the disk halves of python `tests/kernel/test_memory_store.py`
// (`test_named_dedup_survives_a_restart_on_the_same_store`,
// `test_the_id_counter_reserves_blocks_in_the_root_record`).

import FantasticJSON
import FantasticKernel
import FantasticKernelStartup
import Foundation
import Testing

@Suite("restart — records, the id counter, the memory store")
struct RestartTests {
    private func workdir() throws -> URL {
        let url = URL(fileURLWithPath: NSTemporaryDirectory())
            .appendingPathComponent("restart-\(UUID().uuidString)")
        try FileManager.default.createDirectory(at: url, withIntermediateDirectories: true)
        return url
    }

    private func send(_ k: Kernel, _ target: String, _ payload: JSON, id: String? = nil) async
        -> Outcome
    {
        await k.send([Item(AgentId(target), payload, id: id)])[0]
    }

    /// A root with the real record store wired and a `ram_store` named as the
    /// root's memory store — created on the FIRST boot only.
    private func bootWithStore(_ wd: URL, create: Bool) async throws -> Kernel {
        let k = try await startKernel(workdir: wd.path, portHint: 0)
        if create {
            // THE ROOT BOOTS FIRST: its record writer subscribes, so every record
            // created below lands on disk through the discovered provider.
            _ = await send(k, "kernel_state", .object(["type": .string("boot")]))
            let made = await send(k, "kernel_state", .object([
                "type": .string("create_agent"), "parent_id": .string("kernel_state"),
                "handler_module": .string("file_bridge.tools"), "id": .string("store"),
                "root": .string(wd.appendingPathComponent(".fantastic").path),
                "ingress_rule": .string("allow_all_ingress"),
            ])).result
            #expect(made["error"].isNull, "\(made.serialize())")
            let mem = await send(k, "kernel_state", .object([
                "type": .string("create_agent"), "parent_id": .string("kernel_state"),
                "handler_module": .string("ram_store.tools"), "id": .string("memory"),
                "operator": .bool(true), "admins": .array([.string("kernel")]),
            ])).result
            #expect(mem["error"].isNull, "\(mem.serialize())")
            let ex = await send(k, "kernel_state", .object([
                "type": .string("create_agent"), "parent_id": .string("kernel_state"),
                "handler_module": .string("text_file.tools"), "id": .string("notes"),
            ])).result
            #expect(ex["error"].isNull, "\(ex.serialize())")
            k.update(AgentId("kernel_state"), ["memory_store_id": .string("memory")])
        }
        return k
    }

    private func rootRecord(_ wd: URL) throws -> JSON {
        let text = try String(
            contentsOf: wd.appendingPathComponent(".fantastic/agent.yaml"), encoding: .utf8)
        return try YamlIO.load(text)
    }

    @Test func theIdCounterPersistsInTheRootRecordAndResumesPastTheBlock() async throws {
        let wd = try workdir()
        defer { try? FileManager.default.removeItem(at: wd) }
        let k = try await bootWithStore(wd, create: true)
        let out = await k.start(bootAll: false)
        #expect(out["started"].asBool == true, "\(out.serialize())")
        for _ in 0..<(Kernel.ID_BLOCK + 5) { _ = k.mintId() }
        let bound = try #require(k.root?.metaValue(forKey: "next_id")?.asInt)
        await k.shutdown()
        let rec = try rootRecord(wd)
        #expect(rec["next_id"].asInt == bound, "the bound reached agent.yaml: \(rec.serialize())")
        #expect(rec["memory_store_id"].asString == "memory")
        #expect(rec["guid"].asString?.isEmpty == false)
        let k2 = try await startKernel(workdir: wd.path, portHint: 0)
        #expect(k2.root?.metaValue(forKey: "guid") == rec["guid"])
        #expect(k2.mintId() >= bound, "a reboot resumes past the reserved block")
        await k2.shutdown()
    }

    @Test func aNamedExecutionDedupsAcrossARestartOnTheSameStore() async throws {
        let wd = try workdir()
        defer { try? FileManager.default.removeItem(at: wd) }
        let k = try await bootWithStore(wd, create: true)
        #expect((await k.start(bootAll: true))["started"].asBool == true)
        let w = await send(k, "notes", .object(["type": .string("reflect")]), id: "nightly")
        #expect(w.status == .completed, "\(w.result.serialize())")
        await k.flushExecutions()
        let tables = await send(k, "memory", .object(["type": .string("dump")])).result["tables"]
        await k.flushRecords()
        await k.shutdown()
        // A NEW PROCESS: the records come back from disk (guid + counter), the
        // RAM store is empty until the harness injects what it dumped.
        let k2 = try await bootWithStore(wd, create: false)
        let inj = await send(k2, "memory", .object(["type": .string("inject"), "tables": tables]))
        #expect(inj.result["error"].isNull, "\(inj.result.serialize())")
        #expect((await k2.start(bootAll: false))["started"].asBool == true)
        let again = await send(k2, "notes", .object(["type": .string("reflect")]), id: "nightly")
        #expect(again.pid?.value == w.pid?.value, "the recorded execution answered, not a new one")
        #expect(again.result == w.result)
        await k2.shutdown()
    }

    @Test func aDaemonRootWithoutAStoreDoesNotStart() async throws {
        let wd = try workdir()
        defer { try? FileManager.default.removeItem(at: wd) }
        let k = try await startKernel(workdir: wd.path, portHint: 0)
        let out = await k.start(bootAll: true)
        #expect(out["reason"].asString == "missing_arg", "\(out.serialize())")
        #expect(out["hint"].asString?.contains("create a ram_store") == true)
        #expect(!k.started)
        await k.shutdown()
    }

    /// The record writer's final flush runs in `flushPending` — before the
    /// walk, while the provider is alive — so a create+update immediately
    /// followed by a shutdown is on disk.
    @Test func aRecordUpdatedJustBeforeShutdownAlwaysLands() async throws {
        let wd = try workdir()
        defer { try? FileManager.default.removeItem(at: wd) }
        let k = try await bootWithStore(wd, create: true)
        _ = await send(k, "kernel_state", .object(["type": .string("boot")]))
        _ = await send(k, "kernel_state", .object([
            "type": .string("create_agent"), "parent_id": .string("kernel_state"),
            "handler_module": .string("file_bridge.tools"), "id": .string("sp_late"),
            "root": .string(wd.appendingPathComponent("late").path),
            "ingress_rule": .string("allow_all_ingress"),
        ]))
        _ = await send(k, "kernel_state", .object([
            "type": .string("update_agent"), "id": .string("sp_late"),
            "grants": .object(["can_call": .array([.string("store")])]),
        ]))
        await k.shutdown()
        let found =
            (FileManager.default.enumerator(at: wd, includingPropertiesForKeys: nil)?
            .compactMap { $0 as? URL }
            .filter { $0.lastPathComponent == "agent.yaml" && $0.path.contains("sp_late") }) ?? []
        let f = try #require(found.first, "the record never reached disk")
        let doc = try YamlIO.load(String(decoding: try Data(contentsOf: f), as: UTF8.self))
        #expect(doc["grants"]["can_call"] == .array([.string("store")]), "\(doc.serialize())")
        #expect(
            !FileManager.default.fileExists(
                atPath: wd.appendingPathComponent(".fantastic/messages.yaml").path),
            "the kernel writes no message log")
    }
}
