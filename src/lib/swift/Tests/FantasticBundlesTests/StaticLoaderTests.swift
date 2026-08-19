// static_loader — declarative YAML roster hydration + the deny-all closure.
// Mirror of python `test_static_loader.py` and rust `fantastic-static-loader`.

import FantasticJSON
import FantasticKernel
import FantasticKernelStartup
import Foundation
import Testing

@testable import FantasticStaticLoader

private let MANIFEST = """
    agents:
      - id: memory
        handler_module: proxy_agent.tools
      - id: worker
        handler_module: proxy_agent.tools
        can_call:
          memory: [set, read]
    """

private func mkKernel() -> Kernel {
    let kernel = Kernel(storage: .inMemory, bundles: defaultBundleRegistry())
    let root = Agent(id: AgentId("core"), handlerModule: nil, parentId: nil)
    kernel.register(root)
    kernel.setRoot(root)
    return kernel
}

private func writeManifest(_ text: String) -> String {
    let dir = FileManager.default.temporaryDirectory
        .appendingPathComponent("static-loader-\(UUID().uuidString)")
    try? FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
    let file = dir.appendingPathComponent("fantastic.yaml")
    try? text.write(to: file, atomically: true, encoding: .utf8)
    return file.path
}

private func bootLoader(_ text: String) async -> Kernel {
    let kernel = mkKernel()
    let path = writeManifest(text)
    // Operator send (top-level → ungated); create_agent auto-boots the loader,
    // which hydrates the roster.
    let rec = await kernel.send(
        "core",
        [
            "type": "create_agent",
            "handler_module": .string(StaticLoaderBundle.handlerModule),
            "id": "static_loader",
            "manifest": .string(path),
        ])
    #expect(rec["id"].asString == "static_loader", "loader create: \(rec.serialize())")
    return kernel
}

@Test func rosterIsHydratedAsChildrenOfTheLoader() async {
    let kernel = await bootLoader(MANIFEST)
    #expect(kernel.agent(AgentId("memory")) != nil)
    #expect(kernel.agent(AgentId("worker")) != nil)
    for id in ["memory", "worker"] {
        #expect(kernel.agent(AgentId(id))?.parentId?.value == "static_loader")
    }
}

@Test func declaredAgentsCarryBaselinePlusExplicitGrants() async {
    let kernel = await bootLoader(MANIFEST)
    let can = kernel.agent(AgentId("worker"))?.metaValue(forKey: "can_call") ?? .null
    #expect(can["$self"].asString == "*")
    #expect(can["$parent"].asString == "*")
    #expect(can["$children"].asString == "*")
    #expect(can["memory"].asArray?.compactMap { $0.asString } == ["set", "read"])
    // memory got the baseline with no explicit can_call in the manifest.
    let mem = kernel.agent(AgentId("memory"))?.metaValue(forKey: "can_call") ?? .null
    #expect(mem["$self"].asString == "*")
}

@Test func rosterIsClosedAChildCannotCreateMore() async {
    let kernel = await bootLoader(MANIFEST)
    // deny-all: a roster agent is not granted create_agent on anyone.
    let r = await kernel.sendAs(
        sender: "worker", target: "core",
        payload: ["type": "create_agent", "handler_module": "proxy_agent.tools"])
    #expect(r["reason"].asString == "unauthorized", "\(r.serialize())")
}

@Test func bootIsIdempotentById() async {
    let kernel = await bootLoader(MANIFEST)
    let before = kernel.allAgents().count
    let r = await kernel.send("static_loader", ["type": "boot"])
    #expect(r["booted"].asBool == true, "\(r.serialize())")
    let skipped = Set((r["skipped"].asArray ?? []).compactMap { $0.asString })
    #expect(skipped == ["memory", "worker"])
    #expect((r["created"].asArray ?? []).isEmpty)
    #expect(kernel.allAgents().count == before)
}

@Test func missingManifestReportsNotCrashes() async {
    let kernel = mkKernel()
    let rec = await kernel.send(
        "core",
        [
            "type": "create_agent",
            "handler_module": .string(StaticLoaderBundle.handlerModule),
            "id": "sl2", "manifest": "/no/such/path/nope.yaml",
        ])
    #expect(rec["id"].asString == "sl2", "\(rec.serialize())")
    let b = await kernel.send("sl2", ["type": "boot"])
    #expect(b["booted"].asBool == false)
    #expect(b["error"].asString?.contains("cannot read manifest") == true)
}
