// C35: a persisted `alias` meta re-wires on load — the loader rehydrates the
// record and `Kernel.register` sees the alias again. Mirrors python
// `PT/kernel/test_alias.py::test_alias_survives_reboot`.

import FantasticJSON
import FantasticKernel
import FantasticKernelStartup
import Foundation
import Testing

@Suite("C35: alias survives reboot")
struct AliasRebootTests {
    @Test func aliasSurvivesReboot() async throws {
        let tmp = makeTempDir()
        defer { try? FileManager.default.removeItem(at: tmp) }

        let k1 = try await startKernel(workdir: tmp.path)
        _ = await k1.send([Item(AgentId("kernel"), .object(["type": .string("boot")]))])[0]
        let child = Agent(
            id: AgentId("aliased"), handlerModule: nil, parentId: AgentId("kernel_state"),
            meta: ["alias": .string("myname")],
            rootPath: tmp.appendingPathComponent(".fantastic/agents/aliased"))
        k1.register(child)
        try Persistence.persist(agent: k1.root!, storage: .disk(tmp))
        try Persistence.persist(agent: child, storage: .disk(tmp))

        // Second boot of the SAME dir must rehydrate the record — and
        // `register` must see its `alias` meta again.
        let k2 = try await startKernel(workdir: tmp.path)
        #expect(k2.resolveAgent("myname")?.id.value == "aliased")
    }
}

private func makeTempDir() -> URL {
    let url = FileManager.default.temporaryDirectory.appendingPathComponent(
        "fantastic-alias-reboot-test-\(UUID().uuidString)")
    try? FileManager.default.createDirectory(at: url, withIntermediateDirectories: true)
    return url
}
