// A ROUTING NAME A VERB ALSO TAKES: `title` on a call is routing,
// never sent to the target. A verb whose docs name `title` could never receive it —
// the call is refused, loud, and a verb without one is untouched. Twin of python's
// `test_routing_clash.py`.

import FantasticJSON
import FantasticKernel
import Foundation
import OrderedCollections
import Testing

@testable import FantasticAICore

final class ClashStore: AgentBundle, @unchecked Sendable {
    let name = "clash_store"
    private let lock = NSLock()
    private var seen: [String] = []
    var verbDocTable: JSON {
        .object([
            "create": .string("args: title:str (req), body:str (req). A page → {created}."),
            "ping": .string("No args. → {pong}."),
        ])
    }
    func handle(agentId: AgentId, payload: JSON, kernel: Kernel) async throws -> JSON? {
        let verb = payload["type"].asString ?? ""
        if verb == "boot" || verb == "shutdown" || verb == "reflect" { return nil }
        record(verb)
        return .object(["ok": .string(verb)])
    }
    private func record(_ verb: String) {
        lock.lock()
        seen.append(verb)
        lock.unlock()
    }
    func verbs() -> [String] {
        lock.lock()
        defer { lock.unlock() }
        return seen
    }
}

@Suite("ai_core routing clash")
struct RoutingClashTests {
    @Test func aRoutingNameTheVerbDocumentsIsRefusedLoud() async throws {
        let create = #"<call name="send"><target_id>kb</target_id><title>Notes</title><type>create</type><body>b</body></call>"#
        let ping = #"<call name="send"><target_id>kb</target_id><title>check kb</title><type>ping</type></call>"#
        let mine = ProviderScript([[create + "\n" + ping], ["saw it " + doneEnvelope]])
        let (kernel, ids, _) = await bootMockKernel(
            config: scriptedConfig(["mock": mine]), backendIds: ["mock"])
        let store = ClashStore()
        kernel.bundles.register("clash_store.tools", store)
        let mock = ids["mock"]!
        _ = await kernel.send([Item(AgentId("kernel_state"), .object([
            "type": "create_agent", "parent_id": "kernel_state",
            "handler_module": "clash_store.tools", "id": "kb",
            "callable_by": .object([mock.value: "*"]),
        ]))])[0].result
        var cc = kernel.agent(mock)?.metaValue(forKey: "can_call")?.asObject ?? [:]
        cc["kb"] = "*"
        _ = await kernel.send([Item(AgentId("kernel_state"), .object([
            "type": "update_agent", "id": .string(mock.value), "can_call": .object(cc),
        ]))])[0].result
        _ = await kernel.send([Item(mock, .object([
            "type": "send", "text": "go", "client_id": "c",
        ]))])[0].result
        #expect(await mine.waitForCalls(2), "the refusal never reached the model")
        let turn = mine.requestText(1)
        #expect(turn.contains("title on this call is call routing"), "\(turn.suffix(500))")
        #expect(turn.contains("documents an argument named title"), "\(turn.suffix(500))")
        #expect(store.verbs() == ["ping"], "create must not run: \(store.verbs())")
    }
}
