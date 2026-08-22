// The loader lays a program down as its own file.
//
// A record field that is PROGRAM TEXT round-trips through `main.py` beside
// `agent.json` instead of being embedded as an escaped JSON string. Nothing about
// the in-memory record changes — a bundle still sets `code` in meta and reads it
// back — so a storeless kernel behaves exactly as before. Mirrors python's
// `kernel_state` split and rust's `persistence::CODE_FILE`.

import FantasticJSON
import Foundation
import Testing

@testable import FantasticKernel

private let PROG = "import sys\nprint('hello from main')\n"

private func tmpdir() -> URL {
    let d = FileManager.default.temporaryDirectory
        .appendingPathComponent("codesplit-\(UUID().uuidString)")
    try? FileManager.default.createDirectory(at: d, withIntermediateDirectories: true)
    return d
}

private func agentWith(code: String?, id: String = "tool", at root: URL) -> Agent {
    let a = Agent(
        id: AgentId(id), handlerModule: "exec_bridge.tools", parentId: nil, rootPath: root)
    if let code { _ = a.updateMeta(["code": .string(code)]) }
    return a
}

@Suite("loader code split")
struct CodeSplitTests {

    @Test func programLandsBesideTheRecordNotInsideIt() throws {
        let dir = tmpdir()
        let agent = agentWith(code: PROG, at: dir)
        try Persistence.persist(agent: agent, storage: .disk(dir))

        let mainPy = dir.appendingPathComponent("main.py")
        #expect(try String(contentsOf: mainPy, encoding: .utf8) == PROG)

        let raw = try Data(contentsOf: dir.appendingPathComponent("agent.json"))
        guard case .object(let rec) = try JSON.parse(raw) else {
            Issue.record("agent.json is not an object")
            return
        }
        #expect(rec["code"] == nil, "program must not be embedded in agent.json")
    }

    @Test func hydrationFoldsTheProgramBackOntoTheRecord() throws {
        // readAllAgentRecords walks <agentsDir>/<id>/, so persist one level down.
        let agentsDir = tmpdir()
        let dir = agentsDir.appendingPathComponent("tool")
        try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        try Persistence.persist(
            agent: agentWith(code: PROG, at: dir), storage: .disk(dir))

        let recs = Persistence.readAllAgentRecords(from: agentsDir)
        let tool = recs.first { $0.id == "tool" }
        #expect(tool?.meta["code"] == .string(PROG))
    }

    @Test func anAgentWithoutAProgramGetsNoFileAndNoKey() throws {
        let agentsDir = tmpdir()
        let dir = agentsDir.appendingPathComponent("plain")
        try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        let a = Agent(
            id: AgentId("plain"), handlerModule: "cli.tools", parentId: nil, rootPath: dir)
        try Persistence.persist(agent: a, storage: .disk(dir))

        #expect(!FileManager.default.fileExists(atPath: dir.appendingPathComponent("main.py").path))
        let recs = Persistence.readAllAgentRecords(from: agentsDir)
        #expect(recs.first { $0.id == "plain" }?.meta["code"] == nil)
    }

    @Test func emptyCodeIsNotProgramTextAndInventsNoFile() throws {
        let dir = tmpdir()
        try Persistence.persist(
            agent: agentWith(code: "", at: dir), storage: .disk(dir))
        #expect(!FileManager.default.fileExists(atPath: dir.appendingPathComponent("main.py").path))
    }
}
