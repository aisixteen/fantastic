// Open an edge on BOTH ends, as an operator would — python `_testkit.wire`.
// The minimal gate stamps only `{$self: "*"}`: every other
// edge a test drives is an explicit grant.

import FantasticJSON
@testable import FantasticKernel
import OrderedCollections

/// `sender.can_call[target] = verbs` and `target.callable_by[sender] = verbs`,
/// merged into the live records. `verbs` is `"*"` (every verb) or a list.
func wire(_ k: Kernel, _ sender: String, _ target: String, _ verbs: JSON = "*") {
    for (holder, field, other) in [(sender, "can_call", target), (target, "callable_by", sender)] {
        guard let a = k.agent(AgentId(holder)) else { continue }
        var cur = a.metaValue(forKey: field)?.asObject ?? [:]
        cur[other] = verbs
        k.update(AgentId(holder), [field: .object(cur)])
    }
}

extension Kernel {
    /// The test standing in for `sender`'s HANDLER — python `_testkit.as_agent`:
    /// one send that is `sender`'s, exactly as when the kernel runs that agent's
    /// own code (`asSelf`, the dispatch identity — the only identity there is).
    /// A test driver, never a product path.
    func sendAs(sender: AgentId, target: AgentId, payload: JSON) async -> JSON {
        await asSelf(sender) { await send([Item(target, payload)])[0].result }
    }
}
