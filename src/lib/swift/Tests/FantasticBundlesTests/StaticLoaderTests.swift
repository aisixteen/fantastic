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

/// A manifest UNDER THE RUNNING DIR, and the RELATIVE path to it.
///
/// Under, because the manifest read obeys the running-dir law: `fs` is the one
/// disk surface and the cold bring-up read is not exempt, so a manifest in the
/// system temp dir is refused (python and rust behave the same). These tests
/// cannot chdir — sibling suites share the process — so the file comes to the
/// cwd instead of the cwd going to it, exactly as rust's loader tests do.
///
/// Returned with a cleanup closure: a leaked fixture is a directory that
/// reappears in `git status` on every run.
private func writeManifest(_ text: String) -> (path: String, cleanup: () -> Void) {
    let name = ".sl_fixture_\(UUID().uuidString.prefix(8))"
    let dir = URL(fileURLWithPath: FileManager.default.currentDirectoryPath)
        .appendingPathComponent(name)
    try? FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
    let file = dir.appendingPathComponent("fantastic.yaml")
    try? text.write(to: file, atomically: true, encoding: .utf8)
    return ("\(name)/fantastic.yaml", { try? FileManager.default.removeItem(at: dir) })
}

/// The fixture outlives this call: a test that re-boots the loader needs the
/// manifest still on disk, so the CALLER owns the cleanup.
private func bootLoader(_ text: String) async -> (Kernel, () -> Void) {
    let kernel = mkKernel()
    let (path, cleanup) = writeManifest(text)
    // Operator send (top-level → ungated); create_agent auto-boots the loader,
    // which hydrates the roster.
    let rec = await kernel.send([Item("core", [
            "type": "create_agent",
            "parent_id": "core",
            "handler_module": .string(StaticLoaderBundle.handlerModule),
            "id": "static_loader",
            "manifest": .string(path),
        ])])[0].result
    #expect(rec["id"].asString == "static_loader", "loader create: \(rec.serialize())")
    return (kernel, cleanup)
}

@Test func rosterIsHydratedAsChildrenOfTheLoader() async {
    let (kernel, cleanup) = await bootLoader(MANIFEST)
    defer { cleanup() }
    #expect(kernel.agent(AgentId("memory")) != nil)
    #expect(kernel.agent(AgentId("worker")) != nil)
    for id in ["memory", "worker"] {
        #expect(kernel.agent(AgentId(id))?.parentId?.value == "static_loader")
    }
}

@Test func declaredAgentsCarryBaselinePlusExplicitGrants() async {
    let (kernel, cleanup) = await bootLoader(MANIFEST)
    defer { cleanup() }
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
    let (kernel, cleanup) = await bootLoader(MANIFEST)
    defer { cleanup() }
    // deny-all: a roster agent is not granted create_agent on anyone.
    let r = await kernel.sendAs(
        sender: "worker", target: "core",
        payload: ["type": "create_agent", "parent_id": "core", "handler_module": "proxy_agent.tools"])
    #expect(r["reason"].asString == "unauthorized", "\(r.serialize())")
}

@Test func bootIsIdempotentById() async {
    let (kernel, cleanup) = await bootLoader(MANIFEST)
    defer { cleanup() }
    let before = kernel.allAgents().count
    let r = await kernel.send([Item("static_loader", ["type": "boot"])])[0].result
    #expect(r["booted"].asBool == true, "\(r.serialize())")
    let reapplied = Set((r["reapplied"].asArray ?? []).compactMap { $0.asString })
    #expect(reapplied == ["memory", "worker"])
    #expect((r["created"].asArray ?? []).isEmpty)
    #expect(kernel.allAgents().count == before)
}

@Test func anUnreadableManifestIsRefusedAtTheWRITE() async {
    // A broken roster never becomes a booting record: `validateMeta` reads the
    // manifest at create/update time, so the refusal lands on the WRITE and no
    // agent is made. This runtime used to accept the record and only complain
    // at boot — python and rust both refused it here, and the shared agentic
    // corpus is what surfaced the difference (2026-08-28).
    //
    // The path is also outside the running dir, which is a second reason to
    // refuse it: the manifest read is clamped, cold bring-up included.
    let kernel = mkKernel()
    let before = kernel.allAgents().count
    let rec = await kernel.send([Item("core", [
            "type": "create_agent",
            "parent_id": "core",
            "handler_module": .string(StaticLoaderBundle.handlerModule),
            "id": "sl2", "manifest": "/no/such/path/nope.yaml",
        ])])[0].result
    #expect(rec["reason"].asString == "invalid_meta", "\(rec.serialize())")
    #expect(rec["error"].asString?.contains("not readable") == true)
    #expect(kernel.agent("sl2") == nil, "a doomed record was created anyway")
    #expect(kernel.allAgents().count == before)
}

@Test func aRosterEntryWithoutAnIdIsRefusedLoud() async {
    // #105 — the roster is keyed by id, so an id-less entry can never match the
    // agent the LAST boot minted: every reboot created ANOTHER one beside it and
    // the flush persisted them all, with no word to the operator.
    let (kernel, cleanup) = await bootLoader(
        """
        agents:
          - handler_module: proxy_agent.tools
        """)
    defer { cleanup() }
    let before = kernel.allAgents().count
    let r = await kernel.send([Item("static_loader", ["type": "boot"])])[0].result
    #expect(r["booted"].asBool == false, "\(r.serialize())")
    let err = r["error"].asString ?? ""
    #expect(err.contains("explicit id"), "\(err)")
    #expect(err.contains("proxy_agent.tools"), "the entry is named: \(err)")
    // NOTHING was created — not on this boot, and not by the create-time boot.
    #expect(kernel.allAgents().count == before)
    #expect(before == 2, "just the root + the loader itself")
}

@Test func anIdLessEntryIsRefusedBeforeTheValidOnesHydrate() async {
    // The refusal is a REFUSAL, not a skip: boot stops at the bad entry.
    let (kernel, cleanup) = await bootLoader(
        """
        agents:
          - handler_module: proxy_agent.tools
          - id: memory
            handler_module: proxy_agent.tools
        """)
    defer { cleanup() }
    let r = await kernel.send([Item("static_loader", ["type": "boot"])])[0].result
    #expect(r["booted"].asBool == false, "\(r.serialize())")
    #expect(kernel.agent(AgentId("memory")) == nil)
}
