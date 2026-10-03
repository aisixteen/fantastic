// The record's own `system_prompt` (owner 2026-10-03, option b): set on an AI
// agent's record it REPLACES the default system block on EVERY turn of that agent —
// a main turn, a self-delivery, a turn woken by a reply. Precedence: the per-call
// `send` argument (that delivery) > the record field > the default block; read LIVE
// at each turn. Twins of python's ai_core record-system-prompt tests; the write-time
// refusal is pinned in `FantasticAIBackendsTests/RecordSchemaTests`.

import FantasticJSON
import FantasticKernel
import Foundation
import OrderedCollections
import Testing

@testable import FantasticAICore

/// A cheap call whose reply wakes the agent for a second turn.
private let reflectCall =
    #"<call name="send"><target_id>kernel_state</target_id><type>reflect</type><tree>none</tree></call>"#

/// The default block's own heading — present only when the default is used.
private let defaultMarker = "Reaching other agents"

@Suite("ai_core record system_prompt")
struct RecordSystemPromptTests {

    private func send(
        _ kernel: Kernel, _ id: AgentId, _ extra: OrderedDictionary<String, JSON> = [:]
    ) async -> JSON {
        var payload: OrderedDictionary<String, JSON> = [
            "type": "send", "text": "go", "client_id": "c",
        ]
        for (k, v) in extra { payload[k] = v }
        return await kernel.send([Item(id, .object(payload))])[0].result
    }

    private func update(_ kernel: Kernel, _ id: AgentId, _ v: JSON) async -> JSON {
        await kernel.send([
            Item(
                AgentId("kernel_state"),
                .object(["type": "update_agent", "id": .string(id.value), "system_prompt": v]))
        ])[0].result
    }

    /// The record's prompt is the system block of the main turn AND of the turn
    /// the call's reply wakes — no default block on either.
    @Test func theRecordPromptRulesTheMainTurnAndTheReplyWokenTurn() async throws {
        let script = ProviderScript([[reflectCall], ["seen it " + doneEnvelope]])
        let (kernel, ids, _) = await bootMockKernel(
            config: scriptedConfig(["mock": script]), backendIds: ["mock"],
            meta: ["system_prompt": "RECORD BLOCK: you are the record's agent."])
        _ = await send(kernel, ids["mock"]!)
        #expect(await script.waitForCalls(2), "the reply never woke a second turn")
        for n in [0, 1] {
            let sys = script.request(n)?.first?["content"].asString ?? ""
            #expect(sys == "RECORD BLOCK: you are the record's agent.", "pass \(n + 1): \(sys)")
            #expect(!script.requestText(n).contains(defaultMarker), "pass \(n + 1)")
        }
    }

    /// The per-call argument beats the record — for that delivery and the chain it
    /// opened; the record field is untouched.
    @Test func thePerCallArgumentBeatsTheRecord() async throws {
        let script = ProviderScript([[reflectCall], ["seen it " + doneEnvelope]])
        let (kernel, ids, _) = await bootMockKernel(
            config: scriptedConfig(["mock": script]), backendIds: ["mock"],
            meta: ["system_prompt": "RECORD BLOCK"])
        _ = await send(kernel, ids["mock"]!, ["system_prompt": "CALLER BLOCK"])
        #expect(await script.waitForCalls(2))
        for n in [0, 1] {
            let sys = script.request(n)?.first?["content"].asString ?? ""
            #expect(sys == "CALLER BLOCK", "pass \(n + 1): \(sys)")
        }
        #expect(kernel.agent(ids["mock"]!)?.metaValue(forKey: "system_prompt") == "RECORD BLOCK")
    }

    /// Read LIVE: an `update_agent` lands on the very next turn, and a null clears
    /// it — the default block again.
    @Test func anUpdateTakesEffectOnTheNextTurnAndNullClearsIt() async throws {
        let script = ProviderScript([[doneEnvelope], [doneEnvelope], [doneEnvelope]])
        let (kernel, ids, _) = await bootMockKernel(
            config: scriptedConfig(["mock": script]), backendIds: ["mock"])
        let id = ids["mock"]!
        _ = await send(kernel, id)
        #expect(await script.waitForCalls(1))
        #expect(script.requestText(0).contains(defaultMarker), "no record prompt: the default")

        let r = await update(kernel, id, "LATER BLOCK")
        #expect(r["error"].isNull, "\(r)")
        _ = await send(kernel, id)
        #expect(await script.waitForCalls(2))
        #expect(script.request(1)?.first?["content"].asString == "LATER BLOCK")

        _ = await update(kernel, id, .null)
        _ = await send(kernel, id)
        #expect(await script.waitForCalls(3))
        #expect(script.requestText(2).contains(defaultMarker), "null cleared it")
        #expect(!script.requestText(2).contains("LATER BLOCK"))
    }
}
