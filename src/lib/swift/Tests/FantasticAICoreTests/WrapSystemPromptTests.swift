// `pre_system_prompt` / `post_system_prompt` (owner, 2026-10-11): record text placed
// BEFORE and AFTER the system block on every turn — whichever block is in force (the
// default, the record's `system_prompt`, a per-call one), never replacing it;
// verbatim, no placeholders. Refused at the write when unusable; null clears. Twins
// of python's `test_wrap_system_prompt.py`.

import FantasticJSON
import FantasticKernel
import Foundation
import OrderedCollections
import Testing

@testable import FantasticAICore

@Suite("ai_core pre/post system prompt")
struct WrapSystemPromptTests {

    private func send(
        _ kernel: Kernel, _ id: AgentId, _ extra: OrderedDictionary<String, JSON> = [:]
    ) async {
        var payload: OrderedDictionary<String, JSON> = [
            "type": "send", "text": "go", "client_id": "c",
        ]
        for (k, v) in extra { payload[k] = v }
        _ = await kernel.send([Item(id, .object(payload))])[0].result
    }

    private func update(
        _ kernel: Kernel, _ id: AgentId, _ fields: OrderedDictionary<String, JSON>
    ) async -> JSON {
        var p: OrderedDictionary<String, JSON> = ["type": "update_agent", "id": .string(id.value)]
        for (k, v) in fields { p[k] = v }
        return await kernel.send([Item(AgentId("kernel_state"), .object(p))])[0].result
    }

    @Test func preAndPostWrapWhicheverBlockIsInForce() async throws {
        let script = ProviderScript([[doneEnvelope], [doneEnvelope], [doneEnvelope], [doneEnvelope]])
        let (kernel, ids, _) = await bootMockKernel(
            config: scriptedConfig(["mock": script]), backendIds: ["mock"],
            meta: ["pre_system_prompt": "PRE {tree}", "post_system_prompt": "POST"])
        let id = ids["mock"]!
        func sys(_ n: Int) -> String { script.request(n)?.first?["content"].asString ?? "" }

        await send(kernel, id)
        #expect(await script.waitForCalls(1))
        let first = sys(0)
        #expect(first.hasPrefix("PRE {tree}\n\n"), "\(first.prefix(80))")  // verbatim
        #expect(first.hasSuffix("\n\nPOST"), "\(first.suffix(80))")
        #expect(first.contains("Reaching other agents"), "the default block stays whole")

        _ = await update(kernel, id, ["system_prompt": "ROLE"])
        await send(kernel, id)
        #expect(await script.waitForCalls(2))
        #expect(sys(1) == "PRE {tree}\n\nROLE\n\nPOST")

        await send(kernel, id, ["system_prompt": "CALL"])
        #expect(await script.waitForCalls(3))
        #expect(sys(2) == "PRE {tree}\n\nCALL\n\nPOST")

        _ = await update(kernel, id, ["pre_system_prompt": .null, "post_system_prompt": .null])
        await send(kernel, id)
        #expect(await script.waitForCalls(4))
        #expect(sys(3) == "ROLE")
    }

    @Test func anUnusableWrapValueIsRefusedAtTheWrite() {
        let sch = RecordSchema.schema([])
        let rec: OrderedDictionary<String, JSON> = ["memory_store_id": "m"]
        for name in ["pre_system_prompt", "post_system_prompt"] {
            for bad: JSON in ["", "   ", 42, ["x"]] {
                let e = RecordSchema.validate(sch, backend: "b", meta: [name: bad], record: rec)
                #expect(e == RecordSchema.wrapPromptRefusal(name), "\(name)=\(bad)")
            }
            for ok: JSON in ["FINE", .null] {
                #expect(RecordSchema.validate(sch, backend: "b", meta: [name: ok], record: rec) == nil)
            }
        }
    }
}
