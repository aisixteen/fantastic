// durable_memory agent — modeless CRUD round-trip, append, disk-is-truth,
// failfast-without-a-leg. Disk-backed (startKernel) so the real YAML file I/O
// through the bound file_bridge is exercised. Mirrors the Python/Rust
// durable_memory tests.

import FantasticDurableMemory
import FantasticJSON
import FantasticKernel
import FantasticKernelStartup
import Foundation
import Testing

@Suite("durable_memory agent")
struct DurableMemoryTests {
    private func tmpDir() -> URL {
        let u = FileManager.default.temporaryDirectory
            .appendingPathComponent("durable-memory-\(UUID().uuidString)")
        try? FileManager.default.createDirectory(at: u, withIntermediateDirectories: true)
        return u
    }

    /// Wire the `.fantastic` store (an OPEN file_bridge) — durable_memory persists
    /// DIRECTLY through it via write_stream/read_stream (raw bytes). Idempotent.
    private func wireStore(_ kernel: Kernel) async {
        _ = await kernel.send([Item("core", [
                "type": "create_agent", "parent_id": "core", "handler_module": "file_bridge.tools", "id": "store",
                "root": ".fantastic", "ingress_rule": "allow_all_ingress",
            ])])[0].result
    }

    /// A durable_memory agent bound to the store via `file_bridge_id`. Its sidecar
    /// I/O runs under operator authority (like py's operator_context), so no
    /// lateral call grant is needed — the store's OWN ingress seal (open above) is
    /// the only gate on the write.
    private func mkMemoryAgent(_ kernel: Kernel, description: String? = nil) async -> AgentId {
        var rec: JSON = [
            "type": "create_agent", "parent_id": "core",
            "handler_module": "durable_memory.tools", "file_bridge_id": "store",
        ]
        if let description { rec["description"] = .string(description) }
        let out = await kernel.send([Item("core", rec)])[0].result
        return AgentId(out["id"].asString ?? "")
    }

    @Test func setReadRoundtrip() async throws {
        let tmp = tmpDir()
        defer { try? FileManager.default.removeItem(at: tmp) }
        let kernel = try await startKernel(workdir: tmp.path)
        await wireStore(kernel)
        let cid = await mkMemoryAgent(kernel)
        let set = await kernel.send([Item(cid, ["type": "set", "key": "user.name", "value": "Ada"])])[0].result
        #expect(set["ok"].asBool == true)
        let r = await kernel.send([Item(cid, ["type": "read", "key": "user.name"])])[0].result
        #expect(r["value"].asString == "Ada")
        let miss = await kernel.send([Item(cid, ["type": "read", "key": "nope"])])[0].result
        #expect(miss["value"].isNull)
        // read without a key is refused.
        let noKey = await kernel.send([Item(cid, ["type": "read"])])[0].result
        #expect(noKey["error"].asString?.contains("key (str) required") == true)
    }

    @Test func keysSortedAndDelete() async throws {
        let tmp = tmpDir()
        defer { try? FileManager.default.removeItem(at: tmp) }
        let kernel = try await startKernel(workdir: tmp.path)
        await wireStore(kernel)
        let cid = await mkMemoryAgent(kernel)
        _ = await kernel.send([Item(cid, ["type": "set", "key": "z", "value": "hello"])])[0].result
        _ = await kernel.send([Item(cid, ["type": "set", "key": "a", "value": .integer(3)])])[0].result
        let keys = await kernel.send([Item(cid, ["type": "keys"])])[0].result
        #expect((keys["keys"].asArray ?? []).compactMap { $0.asString } == ["a", "z"])
        let del = await kernel.send([Item(cid, ["type": "delete", "key": "a"])])[0].result
        #expect(del["deleted"].asBool == true)
        let del2 = await kernel.send([Item(cid, ["type": "delete", "key": "a"])])[0].result
        #expect(del2["deleted"].asBool == false)
    }

    @Test func appendGrowsAListAndRefusesNonList() async throws {
        let tmp = tmpDir()
        defer { try? FileManager.default.removeItem(at: tmp) }
        let kernel = try await startKernel(workdir: tmp.path)
        await wireStore(kernel)
        let cid = await mkMemoryAgent(kernel)
        // absent key ⇒ created as a list.
        let a1 = await kernel.send([Item(cid, ["type": "append", "key": "log", "value": "one"])])[0].result
        #expect(a1["ok"].asBool == true && a1["length"].asInt == 1)
        let a2 = await kernel.send([Item(cid, ["type": "append", "key": "log", "value": "two"])])[0].result
        #expect(a2["length"].asInt == 2)
        let read = await kernel.send([Item(cid, ["type": "read", "key": "log"])])[0].result
        #expect(read["value"].asArray?.compactMap { $0.asString } == ["one", "two"])
        // a non-list key refuses.
        _ = await kernel.send([Item(cid, ["type": "set", "key": "scalar", "value": "x"])])[0].result
        let bad = await kernel.send([Item(cid, ["type": "append", "key": "scalar", "value": "y"])])[0].result
        #expect(bad["error"].asString?.contains("non-list") == true)
    }

    @Test func diskIsTruthAcrossAFreshLoad() async throws {
        let tmp = tmpDir()
        defer { try? FileManager.default.removeItem(at: tmp) }
        let kernel = try await startKernel(workdir: tmp.path)
        await wireStore(kernel)
        let cid = await mkMemoryAgent(kernel)
        _ = await kernel.send([Item(cid, ["type": "set", "key": "user.name", "value": "Ada"])])[0].result
        // The sidecar landed on disk through the leg (state.yaml under .fantastic).
        let yaml = try? String(
            contentsOf: tmp.appendingPathComponent(".fantastic/state.yaml"), encoding: .utf8)
        #expect(yaml?.contains("user.name") == true && yaml?.contains("Ada") == true, "\(yaml ?? "nil")")
    }

    @Test func reflectSurfacesDescriptionKeysAndBinding() async throws {
        let tmp = tmpDir()
        defer { try? FileManager.default.removeItem(at: tmp) }
        let kernel = try await startKernel(workdir: tmp.path)
        await wireStore(kernel)
        let cid = await mkMemoryAgent(kernel, description: "The user's long-term facts.")
        _ = await kernel.send([Item(cid, ["type": "set", "key": "b", "value": 1])])[0].result
        _ = await kernel.send([Item(cid, ["type": "set", "key": "a", "value": 2])])[0].result
        let r = await kernel.send([Item(cid, ["type": "reflect"])])[0].result
        #expect(r["sentence"].asString == "The user's long-term facts.")
        #expect(r["file_bridge_id"].asString == "store")
        #expect((r["keys"].asArray ?? []).compactMap { $0.asString } == ["a", "b"])
        #expect(r["verbs"]["append"].asString != nil)
        #expect(r["verbs"]["set"].asString != nil)
        // Modeless: no `mode` in the reflect.
        #expect(r["mode"].isNull)
    }

    @Test func mutatorsFailfastWithoutAFileBridge() async throws {
        // NO file_bridge_id wired ⇒ set/append/delete REFUSE (no silent RAM); a
        // read degrades to empty (value null).
        let tmp = tmpDir()
        defer { try? FileManager.default.removeItem(at: tmp) }
        let kernel = try await startKernel(workdir: tmp.path)
        let rec = await kernel.send([Item("core", [
                "type": "create_agent", "parent_id": "core", "handler_module": "durable_memory.tools",
            ])])[0].result
        let cid = AgentId(rec["id"].asString ?? "")

        for (verb, payload) in [
            ("set", ["type": "set", "key": "k", "value": "v"] as JSON),
            ("append", ["type": "append", "key": "k", "value": "v"] as JSON),
            ("delete", ["type": "delete", "key": "k"] as JSON),
        ] {
            let r = await kernel.send([Item(cid, payload)])[0].result
            let err = r["error"].asString ?? ""
            // `delete` only persists when the key existed — an empty store never
            // does, so a missing-key delete succeeds without a write. set/append
            // always failfast.
            if verb == "delete" {
                #expect(r["deleted"].asBool == false, "\(r)")
            } else {
                #expect(err.hasPrefix("durable_memory.\(verb):"), "\(verb): \(r)")
                #expect(err.contains("no file_bridge_id wired"), "\(verb) must name the cause: \(r)")
            }
        }
        let read = await kernel.send([Item(cid, ["type": "read", "key": "k"])])[0].result
        #expect(read["value"].isNull)
    }
}
