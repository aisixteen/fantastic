// yaml_state memory agent — CRUD round-trip, state_yaml, mode sentence,
// disk-is-truth. Disk-backed (startKernel) so the YAML file I/O is
// exercised. Mirrors the Python/Rust yaml_state tests.

import FantasticJSON
import FantasticKernel
import FantasticKernelStartup
import FantasticYamlState
import Foundation
import Testing

@Suite("yaml_state memory agent")
struct YamlStateTests {
    private func tmpDir() -> URL {
        let u = FileManager.default.temporaryDirectory
            .appendingPathComponent("yaml-state-\(UUID().uuidString)")
        try? FileManager.default.createDirectory(at: u, withIntermediateDirectories: true)
        return u
    }

    /// Wire the `.fantastic` store (an open file_bridge) — yaml_state persists
    /// THROUGH it (sealed-by-default model; no own disk surface). Idempotent.
    private func wireStore(_ kernel: Kernel) async {
        _ = await kernel.send([Item("core", [
                "type": "create_agent", "parent_id": "core", "handler_module": "file_bridge.tools", "id": "store",
                "root": ".fantastic", "ingress_rule": "allow_all_ingress",
            ])])[0].result
    }

    private func mkMemoryAgent(_ kernel: Kernel, mode: String) async -> AgentId {
        let rec = await kernel.send([Item("core", [
                "type": "create_agent", "parent_id": "core", "handler_module": "yaml_state.tools",
                "mode": .string(mode), "file_bridge_id": "store",
                // Two-sided deny-all: yaml_state persists read/write/delete to its
                // store directly → an explicit egress grant (wiring pairs with granting).
                "can_call": ["store": "*"],
            ])])[0].result
        let id = rec["id"].asString ?? ""
        // Ingress half: open the store's `callable_by` for this memory agent (merged
        // with the store's structural baseline).
        var accept: JSON = kernel.agent("store")?.metaValue(forKey: "callable_by") ?? .object([:])
        if case .object(var map) = accept {
            map[id] = .string("*")
            accept = .object(map)
        }
        _ = await kernel.send([Item("core", ["type": "update_agent", "id": "store", "callable_by": accept])])[0].result
        return AgentId(id)
    }

    @Test func setGetRoundtrip() async throws {
        let tmp = tmpDir()
        defer { try? FileManager.default.removeItem(at: tmp) }
        let kernel = try await startKernel(workdir: tmp.path)
        await wireStore(kernel)
        let cid = await mkMemoryAgent(kernel, mode: "data")
        _ = await kernel.send([Item(cid, ["type": "set", "key": "user.name", "value": "Ada"])])[0].result
        let r = await kernel.send([Item(cid, ["type": "read", "key": "user.name"])])[0].result
        #expect(r["value"].asString == "Ada")
        let miss = await kernel.send([Item(cid, ["type": "read", "key": "nope"])])[0].result
        #expect(miss["value"].isNull)
    }

    @Test func keysSurveySortedAndDelete() async throws {
        let tmp = tmpDir()
        defer { try? FileManager.default.removeItem(at: tmp) }
        let kernel = try await startKernel(workdir: tmp.path)
        await wireStore(kernel)
        let cid = await mkMemoryAgent(kernel, mode: "data")
        _ = await kernel.send([Item(cid, ["type": "set", "key": "z", "value": "hello"])])[0].result
        _ = await kernel.send([Item(cid, ["type": "set", "key": "a", "value": .integer(3)])])[0].result
        let keys = await kernel.send([Item(cid, ["type": "keys"])])[0].result
        let names = (keys["keys"].asArray ?? []).compactMap { $0["key"].asString }
        #expect(names == ["a", "z"])
        // delete
        let del = await kernel.send([Item(cid, ["type": "delete", "key": "a"])])[0].result
        #expect(del["deleted"].asBool == true)
        let del2 = await kernel.send([Item(cid, ["type": "delete", "key": "a"])])[0].result
        #expect(del2["deleted"].asBool == false)
    }

    @Test func replaceAndClear() async throws {
        let tmp = tmpDir()
        defer { try? FileManager.default.removeItem(at: tmp) }
        let kernel = try await startKernel(workdir: tmp.path)
        await wireStore(kernel)
        let cid = await mkMemoryAgent(kernel, mode: "data")
        _ = await kernel.send([Item(cid, ["type": "set", "key": "old", "value": .integer(1)])])[0].result
        _ = await kernel.send([Item(cid, ["type": "replace", "doc": ["new": .integer(2)]])])[0].result
        let doc = await kernel.send([Item(cid, ["type": "read"])])[0].result
        #expect(doc["doc"]["new"].asInt == 2)
        #expect(doc["doc"]["old"].isNull)
        _ = await kernel.send([Item(cid, ["type": "replace", "doc": [:]])])[0].result
        #expect((await kernel.send([Item(cid, ["type": "read"])])[0].result)["doc"].asObject?.isEmpty == true)
    }

    @Test func stateYamlAndDiskIsTruth() async throws {
        let tmp = tmpDir()
        defer { try? FileManager.default.removeItem(at: tmp) }
        let kernel = try await startKernel(workdir: tmp.path)
        await wireStore(kernel)
        let cid = await mkMemoryAgent(kernel, mode: "mem")
        _ = await kernel.send([Item(cid, ["type": "set", "key": "user.name", "value": "Ada"])])[0].result
        let y = await kernel.send([Item(cid, ["type": "state_yaml"])])[0].result
        let text = y["yaml"].asString ?? ""
        #expect(text.contains("user.name") && text.contains("Ada"))
        // empty store → empty string
        let empty = await mkMemoryAgent(kernel, mode: "data")
        #expect((await kernel.send([Item(empty, ["type": "state_yaml"])])[0].result)["yaml"].asString == "")
    }

    @Test func reflectModeSentence() async throws {
        let tmp = tmpDir()
        defer { try? FileManager.default.removeItem(at: tmp) }
        let kernel = try await startKernel(workdir: tmp.path)
        await wireStore(kernel)
        let mem = await mkMemoryAgent(kernel, mode: "mem")
        let data = await mkMemoryAgent(kernel, mode: "data")
        let rmem = await kernel.send([Item(mem, ["type": "reflect"])])[0].result
        let rdata = await kernel.send([Item(data, ["type": "reflect"])])[0].result
        #expect(rmem["mode"].asString == "mem")
        #expect(rmem["sentence"].asString?.contains("durable memory") == true)
        #expect(rdata["mode"].asString == "data")
        #expect(rdata["sentence"].asString?.contains("scratch-state") == true)
        #expect(rmem["verbs"]["set"].asString != nil)
        // reflect surfaces the provider binding.
        #expect(rmem["file_bridge_id"].asString == "store")
    }

    @Test func mutatorsFailWithoutAStore() async throws {
        // NO STORE WIRED → set/delete/replace REFUSE (no silent loss); reads
        // degrade to empty. Persistence now goes THROUGH the loader, so the
        // failure this bundle must surface is "the kernel has no store", not
        // "my own file_bridge_id is unset" — the consumer no longer names a
        // provider at all.
        let tmp = tmpDir()
        defer { try? FileManager.default.removeItem(at: tmp) }
        let kernel = try await startKernel(workdir: tmp.path)
        // NOTE: deliberately NO wireStore.
        let rec = await kernel.send([
            Item(
                "core",
                [
                    "type": "create_agent", "parent_id": "core",
                    "handler_module": "yaml_state.tools", "mode": "data",
                ])
        ])[0].result
        let cid = AgentId(rec["id"].asString ?? "")

        for (verb, payload) in [
            ("set", ["type": "set", "key": "k", "value": "v"] as JSON),
            ("delete", ["type": "delete", "key": "k"] as JSON),
            ("replace", ["type": "replace", "doc": [:]] as JSON),
        ] {
            let r = await kernel.send([Item(cid, payload)])[0].result
            let err = r["error"].asString ?? ""
            #expect(
                err.hasPrefix("yaml_state.\(verb):"),
                "\(verb) must fail in this bundle's own words: \(r)")
            #expect(err.contains("no store wired"), "\(verb) must name the real cause: \(r)")
        }

        // reads degrade to empty, not error.
        let read = await kernel.send([Item(cid, ["type": "read"])])[0].result
        #expect(read["doc"] == .object([:]))
    }
}
