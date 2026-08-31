// FM backend memory: idempotent self-mount of the memory FILE surface + its
// text_file editor at boot, and the load-bearing inject — each memory
// DOCUMENT's content spliced into the model's instructions. Grounded on tool
// output, so it runs without the actual on-device model.

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

    /// Wire the `.fantastic` store so the backend self-wires its own
    /// `file_bridge_id` (the chat sidecar's leg) — the memory surface is the
    /// backend's OWN private file_bridge child, mounted at boot.
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

    @Test func bootSelfMountsMemorySurfaceAndEditorIdempotently() async throws {
        let tmp = tmpDir()
        defer { try? FileManager.default.removeItem(at: tmp) }
        let kernel = try await startKernel(workdir: tmp.path)
        await wireStore(kernel)
        let fmId = await mkBackend(kernel)
        _ = await kernel.send([Item(fmId, ["type": "boot"])])[0].result

        let bundle = FoundationModelsBackendBundle()
        // ONE editor (text_file), wired to the backend's private file surface.
        let editors = bundle.memoryAgents(agentId: fmId, kernel: kernel)
        #expect(editors.map { $0.value } == ["\(fmId.value)_memory"])
        let surface = kernel.agent(editors[0])?.metaValue(forKey: "target_id")?.asString
        #expect(surface == "\(fmId.value)_memory_files")
        #expect(kernel.agent(AgentId(surface ?? ""))?.handlerModule == "file_bridge.tools")

        // Idempotent: a second boot does not add more agents.
        _ = await kernel.send([Item(fmId, ["type": "boot"])])[0].result
        #expect(bundle.memoryAgents(agentId: fmId, kernel: kernel).count == 1)
    }

    @Test func memoryDocumentReachesInstructions() async throws {
        let tmp = tmpDir()
        defer { try? FileManager.default.removeItem(at: tmp) }
        let kernel = try await startKernel(workdir: tmp.path)
        await wireStore(kernel)
        let fmId = await mkBackend(kernel)
        _ = await kernel.send([Item(fmId, ["type": "boot"])])[0].result

        let bundle = FoundationModelsBackendBundle()
        // Write a distinctive memory DOCUMENT through the editor — any format,
        // the model's own choice; nothing is parsed on the way in.
        let ed = bundle.memoryAgents(agentId: fmId, kernel: kernel)[0]
        let w = await kernel.sendWithBinary(
            ed,
            .object(["type": .string("write"), "path": .string("notes.md")]),
            Data("user.name: Ada_Lovelace_42\n".utf8)
        ).0
        #expect(w["error"].asString == nil, "\(w)")

        // The inject hook splices the document's content into the
        // instructions — the distinctive token grounds the test on real tool
        // output, not a prompt marker.
        let fmAgent = kernel.agent(fmId)!
        let inst = await bundle.fullInstructions(agent: fmAgent, kernel: kernel)
        #expect(inst.contains("Ada_Lovelace_42"), "\(inst)")
        #expect(inst.contains("notes.md"))
    }
}
