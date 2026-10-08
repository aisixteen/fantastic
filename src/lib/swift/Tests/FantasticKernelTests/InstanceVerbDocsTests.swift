// PER-INSTANCE VERBS (owner, 2026-10-08) — python
// `test_docs_of_an_agent_read_its_own_table_when_its_bundle_names_one`: a bundle
// whose agents serve different verbs names each one's table (`instanceVerbDocs`);
// nil keeps the bundle's.

import FantasticJSON
import Foundation
import OrderedCollections
import Testing

@testable import FantasticKernel

private struct PerInstance: AgentBundle {
    let name = "per_instance_probe"
    var verbDocTable: JSON { ["ping": "No args. The bundle's own verb."] }
    func instanceVerbDocs(_ agent: Agent) -> JSON? {
        guard agent.id.value != "plain" else { return nil }
        return .object([
            "ping": "No args. Still here.", "own_\(agent.id.value)": "args: x:int. Only mine.",
        ])
    }
    func handle(agentId: AgentId, payload: JSON, kernel: Kernel) async throws -> JSON? { nil }
}

@Suite("docs: an agent's own verb table")
struct InstanceVerbDocsTests {
    @Test func docsOfAnAgentReadItsOwnTableWhenItsBundleNamesOne() async {
        let r = BundleRegistry()
        r.register("per_instance_probe", PerInstance())
        let k = Kernel(storage: .inMemory, bundles: r)
        let root = Agent(id: AgentId("kernel_state"), handlerModule: nil, parentId: nil)
        k.register(root)
        k.setRoot(root)
        for id in ["a1", "a2", "plain"] {
            k.register(
                Agent(id: AgentId(id), handlerModule: "per_instance_probe", parentId: root.id))
        }
        func docs(_ fields: OrderedDictionary<String, JSON>) async -> JSON {
            var p: OrderedDictionary<String, JSON> = ["type": "docs"]
            for (key, v) in fields { p[key] = v }
            return await k.send([Item(AgentId("kernel"), .object(p))])[0].result
        }
        let a1 = await docs(["of": "a1"])
        #expect(a1["verbs"] == ["ping": "No args", "own_a1": "args: x:int"], "\(a1.serialize())")
        let a2 = await docs(["of": "a2", "tier": "detailed", "verb": "own_a2"])
        #expect(a2["verbs"] == ["own_a2": "args: x:int. Only mine."], "\(a2.serialize())")
        #expect(await docs(["of": "plain"])["verbs"] == ["ping": "No args"])
        let gone = await docs(["of": "a1", "verb": "own_a2"])
        #expect(
            gone["reason"] == "bad_arg" && gone["hint"].asString?.contains("own_a1") == true,
            "\(gone.serialize())")
    }
}
