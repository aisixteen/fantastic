// `children_dir` BECOMES A DIRECTORY (`<agent>/<children_dir>/<child>`),
// so python's `create_agent` / `update_agent` hold it to the agent-id rule
// (`guard_agent_id(..., arg="children_dir")`). Only its string-ness was ever
// checked: `../x` was accepted, and the next boot's clamped walk refused the tree.

import FantasticJSON
import Foundation
import OrderedCollections
import Testing

@testable import FantasticKernel

private struct Noop: AgentBundle {
    let name = "noop"
    func handle(agentId: AgentId, payload: JSON, kernel: Kernel) async throws -> JSON? {
        .object(["ok": .bool(true)])
    }
}

@Suite("children_dir is one safe segment at the write")
struct ChildrenDirGuardTests {
    private func makeKernel() -> Kernel {
        let registry = BundleRegistry()
        registry.register("noop.tools", Noop())
        let kernel = Kernel(storage: .inMemory, bundles: registry)
        let root = Agent(id: "kernel_state", handlerModule: nil, parentId: nil)
        kernel.register(root)
        kernel.setRoot(root)
        return kernel
    }

    private func call(_ k: Kernel, _ p: OrderedDictionary<String, JSON>) async -> JSON {
        await k.send([Item(AgentId("kernel"), .object(p))])[0].result
    }

    private static let bad: [(JSON, String)] = [
        (.string("../sibling"), "a single path segment (no separators); got '../sibling'"),
        (.string("a/b"), "a single path segment (no separators); got 'a/b'"),
        (.string(".."), "a name that is not a path traversal; got '..'"),
        (.string(".hidden"), "a name that is not hidden; got '.hidden'"),
        (.string(""), "a non-empty string"),
        (.null, "a non-empty string"),
    ]

    private func refusal(_ verb: String, _ want: String) -> JSON {
        .object([
            "error": .string("\(verb): children_dir must be \(want)"),
            "reason": .string("bad_arg"),
            "arg": .string("children_dir"),
        ])
    }

    @Test func refusedAtCreate() async {
        let k = makeKernel()
        for (value, want) in Self.bad {
            let r = await call(
                k,
                [
                    "type": .string("create_agent"), "parent_id": .string("kernel_state"),
                    "handler_module": .string("noop.tools"), "id": .string("x"),
                    "children_dir": value,
                ])
            #expect(r == refusal("create_agent", want), "\(r.serialize())")
            #expect(k.agent(AgentId("x")) == nil)
        }
        let ok = await call(
            k,
            [
                "type": .string("create_agent"), "parent_id": .string("kernel_state"),
                "handler_module": .string("noop.tools"), "id": .string("x"),
                "children_dir": .string("web_agents"),
            ])
        #expect(ok["children_dir"].asString == "web_agents", "\(ok.serialize())")
    }

    @Test func refusedAtUpdate() async {
        let k = makeKernel()
        for (value, want) in Self.bad {
            let r = await call(
                k,
                [
                    "type": .string("update_agent"), "id": .string("kernel_state"),
                    "children_dir": value,
                ])
            #expect(r == refusal("update_agent", want), "\(r.serialize())")
        }
        let ok = await call(
            k,
            [
                "type": .string("update_agent"), "id": .string("kernel_state"),
                "children_dir": .string("host_agents"),
            ])
        #expect(ok["updated"].asBool == true, "\(ok.serialize())")
        #expect(ok["agent"]["children_dir"].asString == "host_agents", "\(ok.serialize())")
    }
}
