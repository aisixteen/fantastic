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

    private func mkMemoryAgent(_ kernel: Kernel, mode: String) async -> AgentId {
        let rec = await kernel.send(
            "core",
            [
                "type": "create_agent", "handler_module": "yaml_state.tools",
                "mode": .string(mode),
            ])
        return AgentId(rec["id"].asString ?? "")
    }

    @Test func setGetRoundtrip() async throws {
        let tmp = tmpDir()
        defer { try? FileManager.default.removeItem(at: tmp) }
        let kernel = try await startKernel(workdir: tmp.path)
        let cid = await mkMemoryAgent(kernel, mode: "data")
        _ = await kernel.send(cid, ["type": "set", "key": "user.name", "value": "Ada"])
        let r = await kernel.send(cid, ["type": "read", "key": "user.name"])
        #expect(r["value"].asString == "Ada")
        let miss = await kernel.send(cid, ["type": "read", "key": "nope"])
        #expect(miss["value"].isNull)
    }

    @Test func keysSurveySortedAndDelete() async throws {
        let tmp = tmpDir()
        defer { try? FileManager.default.removeItem(at: tmp) }
        let kernel = try await startKernel(workdir: tmp.path)
        let cid = await mkMemoryAgent(kernel, mode: "data")
        _ = await kernel.send(cid, ["type": "set", "key": "z", "value": "hello"])
        _ = await kernel.send(cid, ["type": "set", "key": "a", "value": .integer(3)])
        let keys = await kernel.send(cid, ["type": "keys"])
        let names = (keys["keys"].asArray ?? []).compactMap { $0["key"].asString }
        #expect(names == ["a", "z"])
        // delete
        let del = await kernel.send(cid, ["type": "delete", "key": "a"])
        #expect(del["deleted"].asBool == true)
        let del2 = await kernel.send(cid, ["type": "delete", "key": "a"])
        #expect(del2["deleted"].asBool == false)
    }

    @Test func replaceAndClear() async throws {
        let tmp = tmpDir()
        defer { try? FileManager.default.removeItem(at: tmp) }
        let kernel = try await startKernel(workdir: tmp.path)
        let cid = await mkMemoryAgent(kernel, mode: "data")
        _ = await kernel.send(cid, ["type": "set", "key": "old", "value": .integer(1)])
        _ = await kernel.send(cid, ["type": "replace", "doc": ["new": .integer(2)]])
        let doc = await kernel.send(cid, ["type": "read"])
        #expect(doc["doc"]["new"].asInt == 2)
        #expect(doc["doc"]["old"].isNull)
        _ = await kernel.send(cid, ["type": "replace", "doc": [:]])
        #expect((await kernel.send(cid, ["type": "read"]))["doc"].asObject?.isEmpty == true)
    }

    @Test func stateYamlAndDiskIsTruth() async throws {
        let tmp = tmpDir()
        defer { try? FileManager.default.removeItem(at: tmp) }
        let kernel = try await startKernel(workdir: tmp.path)
        let cid = await mkMemoryAgent(kernel, mode: "mem")
        _ = await kernel.send(cid, ["type": "set", "key": "user.name", "value": "Ada"])
        let y = await kernel.send(cid, ["type": "state_yaml"])
        let text = y["yaml"].asString ?? ""
        #expect(text.contains("user.name") && text.contains("Ada"))
        // empty store → empty string
        let empty = await mkMemoryAgent(kernel, mode: "data")
        #expect((await kernel.send(empty, ["type": "state_yaml"]))["yaml"].asString == "")
    }

    @Test func reflectModeSentence() async throws {
        let tmp = tmpDir()
        defer { try? FileManager.default.removeItem(at: tmp) }
        let kernel = try await startKernel(workdir: tmp.path)
        let mem = await mkMemoryAgent(kernel, mode: "mem")
        let data = await mkMemoryAgent(kernel, mode: "data")
        let rmem = await kernel.send(mem, ["type": "reflect"])
        let rdata = await kernel.send(data, ["type": "reflect"])
        #expect(rmem["mode"].asString == "mem")
        #expect(rmem["sentence"].asString?.contains("durable memory") == true)
        #expect(rdata["mode"].asString == "data")
        #expect(rdata["sentence"].asString?.contains("scratch-state") == true)
        #expect(rmem["verbs"]["set"].asString != nil)
    }
}
