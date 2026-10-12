// Harness text is a NOTICE, not a message — python `test_notice_rows.py`, case for
// case. A turn this layer writes to its own agent is stored as `kind: "notice"`, so
// a view can hide it; the model still reads it as a user turn. Only the agent itself
// can mark one.

import FantasticJSON
@testable import FantasticKernel
import Foundation
import OrderedCollections
import Testing

@testable import FantasticAICore

private let E59 = "<tool_call>\n<function=call name=\"send\">\n<target_id>files</target_id>\n</call>"

private func users(_ kernel: Kernel, _ id: AgentId) -> [JSON] {
    conversationRows(kernel: kernel, id: id).filter { $0["role"].asString == "user" }
}

@Suite("harness text is a notice")
struct NoticeRowsTests {

    @Test func theNoCallReminderIsANoticeAndTheTaskIsNot() async throws {
        let script = ProviderScript([["just prose"], [doneEnvelope]])
        let (kernel, ids, _) = await bootMockKernel(
            config: scriptedConfig(["nt_a": script]), backendIds: ["nt_a"])
        let id = ids.values.first!
        _ = await kernel.send([Item(id, ["type": "send", "text": "go", "client_id": "c"])])[0]
        #expect(await script.waitForCalls(2))
        try await waitForHistory(kernel: kernel, id: id, count: 4)
        let u = users(kernel, id)
        #expect(u[0]["content"] == "go" && u[0]["kind"] == .null, "\(u)")
        #expect(u[1]["content"].asString?.hasPrefix("[no call was dispatched]") == true, "\(u)")
        #expect(u[1]["kind"] == "notice", "\(u)")
    }

    @Test func aReportBatchIsANoticeAndABatchWithAReplyIsNot() async throws {
        let good = "<call name=\"send\">\n<target_id>kernel_state</target_id>\n<type>reflect</type>\n"
            + "<tree>none</tree>\n</call>"
        let script = ProviderScript([
            [E59], [good + "\n<send><target_id>kernel_state</target_id></send>"], [doneEnvelope],
        ])
        let (kernel, ids, _) = await bootMockKernel(
            config: scriptedConfig(["nt_b": script]), backendIds: ["nt_b"])
        let id = ids.values.first!
        _ = await kernel.send([Item(id, ["type": "send", "text": "go", "client_id": "c"])])[0]
        #expect(await script.waitForCalls(3))
        try await waitForHistory(kernel: kernel, id: id, count: 6)
        let u = users(kernel, id)
        #expect(u[1]["kind"] == "notice", "\(u)")
        #expect(u[2]["content"].asString?.contains("kernel_state reflect") == true, "\(u)")
        #expect(u[2]["kind"] == .null, "\(u)")
    }

    @Test func aRefusedAwaitIsANotice() async throws {
        let script = ProviderScript([["<call name=\"await\"></call>"], [doneEnvelope]])
        let (kernel, ids, _) = await bootMockKernel(
            config: scriptedConfig(["nt_c": script]), backendIds: ["nt_c"])
        let id = ids.values.first!
        _ = await kernel.send([Item(id, ["type": "send", "text": "go", "client_id": "c"])])[0]
        #expect(await script.waitForCalls(2))
        try await waitForHistory(kernel: kernel, id: id, count: 4)
        #expect(users(kernel, id)[1]["kind"] == "notice")
    }

    @Test func aPeerCannotMarkItsMessageANotice() async throws {
        let script = ProviderScript([[doneEnvelope]])
        let (kernel, ids, _) = await bootMockKernel(
            config: scriptedConfig(["nt_d": script]), backendIds: ["nt_d"],
            meta: ["callable_by": .object(["$self": "*", "peer_x": "*"])])
        let id = ids.values.first!
        _ = await kernel.send([Item(AgentId("kernel_state"), .object([
            "type": "create_agent", "parent_id": "kernel_state", "id": "peer_x",
            "handler_module": "mock_recorder.tools",
            "can_call": .object([id.value: "*"]),
        ]))])[0]
        let out = await KernelTaskLocals.$sender.withValue(AgentId("peer_x")) {
            await kernel.send([Item(id, ["type": "send", "text": "hidden?", "notice": true])])[0]
        }
        let result = out.result
        #expect(await script.waitForCalls(1), "\(result)")
        try await waitForHistory(kernel: kernel, id: id, count: 2)
        let u = users(kernel, id)
        #expect(u[0]["content"].asString?.hasSuffix("hidden?") == true && u[0]["kind"] == .null, "\(u)")
    }

    @Test func theIncomingRowCarriesNoticeOnlyWhenAsked() {
        #expect(
            incomingRow("t", id: 3, parent: 2, notice: true)
                == .object(["role": "user", "content": "t", "id": 3, "parent": 2, "kind": "notice"]))
        #expect(incomingRow("t", id: 3, parent: nil, notice: false)["kind"] == .null)
    }
}
