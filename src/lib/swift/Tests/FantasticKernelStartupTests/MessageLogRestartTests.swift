// THE DURABLE MESSAGE LOG through the REAL daemon path: a disk-backed kernel
// (`startKernel`), the real `file_bridge` store discovered at `.fantastic`,
// the pump, a shutdown flush, and a FRESH kernel on the same workdir reading
// it all back with the counter continuing. Miniature of `E78_kernel_restart`.

import FantasticJSON
import FantasticKernel
import FantasticKernelStartup
import Foundation
import Testing

@Suite("message log — restart through the real store")
struct MessageLogRestartTests {
    private func workdir() throws -> URL {
        let url = URL(fileURLWithPath: NSTemporaryDirectory())
            .appendingPathComponent("msgrestart-\(UUID().uuidString)")
        try FileManager.default.createDirectory(at: url, withIntermediateDirectories: true)
        return url
    }

    @Test func rowsSurviveARestartThroughTheDiscoveredStore() async throws {
        let wd = try workdir()
        let k1 = try await startKernel(workdir: wd.path, portHint: 0)
        try hydrateMessages(k1, rootDir: wd.appendingPathComponent(".fantastic"))
        // THE STORE: a file_bridge rooted at `.fantastic` — what the loader
        // discovers as its provider (E78 wires exactly this).
        let made = await k1.send([Item(AgentId("core"), .object([
            "type": .string("create_agent"), "parent_id": .string("core"),
            "handler_module": .string("file_bridge.tools"), "id": .string("store"),
            "root": .string(wd.appendingPathComponent(".fantastic").path),
            // SEALED BY DEFAULT: an io leg with no rule denies every inbound
            // call, the loader's own included — E78 opens it exactly so.
            "ingress_rule": .string("allow_all_ingress"),
        ]))])[0].result
        #expect(made["error"].isNull, "\(made.serialize())")
        #expect(k1.findStore()?.value == "store")
        k1.installMessagePump(after: k1.rows.nextId - 1)
        let ai = AgentId("ai")
        let a = try k1.rows.put(ai, ["role": .string("user"), "content": .string("what is the code?")])
        _ = await k1.send([Item(AgentId("kernel"), .object(["type": .string("ps")]))])
        let b = try k1.rows.put(ai, ["role": .string("assistant"), "content": .string("reading it now"), "parent": .integer(a)])
        let nextBefore = k1.rows.nextId
        await k1.flushPendingMessages()
        let log = wd.appendingPathComponent(".fantastic/messages.xml")
        let text = try String(contentsOf: log, encoding: .utf8)
        #expect(text.hasPrefix(MessagesXML.header), "\(text.prefix(80))")
        #expect(!text.contains("agent=\"store\""), "the pump's own store traffic never enters the log")
        #expect(text.contains("kind=\"call\" verb=\"ps\""), "the ps call row landed: \(text)")
        _ = await k1.send([Item(AgentId("web"), .object(["type": .string("stop")]))])

        let k2 = try await startKernel(workdir: wd.path, portHint: 0)
        let n = try hydrateMessages(k2, rootDir: wd.appendingPathComponent(".fantastic"))
        #expect(n >= 2)
        #expect(k2.rows.nextId == nextBefore, "the counter continues above every hydrated id")
        let back = k2.rows.forAgent(ai)
        #expect(back.map { $0["id"]?.asInt } == [a, b])
        #expect(back[1]["parent"]?.asInt == a)
        // The hydrated `rows_read` answers the same rows over the verb.
        let r = await k2.send([Item(AgentId("kernel"), .object(["type": .string("rows_read"), "agent": .string("ai")]))])[0].result
        #expect((r["rows"].asArray ?? []).compactMap { $0["id"].asInt } == [a, b], "\(r.serialize())")
        _ = await k2.send([Item(AgentId("web"), .object(["type": .string("stop")]))])
    }
}
