// FM backend memory: idempotent self-mount of mem/data durable_memory agents at
// boot, and the load-bearing inject — each agent's keys/values spliced into the
// model's instructions. Grounded on tool output (keys+read → fullInstructions),
// so it runs without the actual on-device model.

import FantasticJSON
import FantasticKernel
import FantasticKernelStartup
import Foundation
import Testing

@testable import FantasticFoundationModelsBackend

@Suite("FM memory: self-mount + inject")
struct FmMemoryInjectTests {
    private func tmpDir() -> URL {
        let u = FileManager.default.temporaryDirectory
            .appendingPathComponent("fm-mem-\(UUID().uuidString)")
        try? FileManager.default.createDirectory(at: u, withIntermediateDirectories: true)
        return u
    }

    /// Wire the `.fantastic` store so the self-mounted memory agents discover a
    /// provider (`findStore`) and persist through it (no own disk surface).
    private func wireStore(_ kernel: Kernel) async {
        _ = await kernel.send([Item("core", [
                "type": "create_agent", "parent_id": "core", "handler_module": "file_bridge.tools", "id": "store",
                "root": ".fantastic", "ingress_rule": "allow_all_ingress",
            ])])[0].result
    }

    private func mkBackend(_ kernel: Kernel) async -> AgentId {
        let rec = await kernel.send([Item("core", ["type": "create_agent", "parent_id": "core", "handler_module": "foundation_models_backend.tools"])])[0].result
        return AgentId(rec["id"].asString ?? "")
    }

    @Test func bootSelfMountsMemAndDataIdempotently() async throws {
        let tmp = tmpDir()
        defer { try? FileManager.default.removeItem(at: tmp) }
        let kernel = try await startKernel(workdir: tmp.path)
        await wireStore(kernel)
        let fmId = await mkBackend(kernel)
        _ = await kernel.send([Item(fmId, ["type": "boot"])])[0].result

        let bundle = FoundationModelsBackendBundle()
        let agents = bundle.memoryAgents(agentId: fmId, kernel: kernel)
        #expect(agents.count == 2)
        let modes = Set(agents.compactMap { kernel.agent($0)?.metaValue(forKey: "mode")?.asString })
        #expect(modes == ["mem", "data"])

        // Idempotent: a second boot does not add more agents.
        _ = await kernel.send([Item(fmId, ["type": "boot"])])[0].result
        #expect(bundle.memoryAgents(agentId: fmId, kernel: kernel).count == 2)
    }

    @Test func stateYamlReachesInstructions() async throws {
        let tmp = tmpDir()
        defer { try? FileManager.default.removeItem(at: tmp) }
        let kernel = try await startKernel(workdir: tmp.path)
        await wireStore(kernel)
        let fmId = await mkBackend(kernel)
        _ = await kernel.send([Item(fmId, ["type": "boot"])])[0].result

        let bundle = FoundationModelsBackendBundle()
        // Set a distinctive fact on the mounted `mem` agent.
        let memAgent = bundle.memoryAgents(agentId: fmId, kernel: kernel).first {
            kernel.agent($0)?.metaValue(forKey: "mode")?.asString == "mem"
        }
        #expect(memAgent != nil)
        _ = await kernel.send([Item(memAgent!, ["type": "set", "key": "user.name", "value": "Ada_Lovelace_42"])])[0].result

        // The inject hook must splice the agent's keys/values into the
        // instructions — distinctive token present grounds the test on
        // real tool output, not a prompt marker.
        let fmAgent = kernel.agent(fmId)!
        let inst = await bundle.fullInstructions(agent: fmAgent, kernel: kernel)
        #expect(inst.contains("Ada_Lovelace_42"))
        #expect(inst.contains("user.name"))
    }
}
