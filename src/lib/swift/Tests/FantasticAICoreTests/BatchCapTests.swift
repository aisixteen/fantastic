// The inline cap bounds a turn's WHOLE reply batch, not each reply. Twins of python `ai_core/tests/test_reply_delivery.py`
// (`test_the_inline_cap_bounds_the_whole_batch_not_each_reply`,
// `test_replies_that_fit_together_are_all_inline`).

import FantasticJSON
import FantasticKernel
import Foundation
import OrderedCollections
import Testing

@testable import FantasticAICore

/// Answers any verb `{text: <its id's first letter × chars>}`.
private final class SizedBundle: AgentBundle, @unchecked Sendable {
    let name = "sized"
    let chars: Int
    init(chars: Int) { self.chars = chars }

    func handle(agentId: AgentId, payload: JSON, kernel: Kernel) async throws -> JSON? {
        switch payload["type"].asString ?? "" {
        case "boot", "shutdown": return nil
        default:
            return .object([
                "text": .string(String(repeating: String(agentId.value.prefix(1)), count: chars))
            ])
        }
    }
}

/// Records every delivered `(agent, payload)`.
private final class Deliveries: AgentBundle, @unchecked Sendable {
    let name = "deliveries"
    private let lock = NSLock()
    private var seen: [(String, JSON)] = []

    func handle(agentId: AgentId, payload: JSON, kernel: Kernel) async throws -> JSON? {
        switch payload["type"].asString ?? "" {
        case "boot", "shutdown": return nil
        default:
            lock.withLock { seen.append((agentId.value, payload)) }
            return .object(["recorded": .bool(true)])
        }
    }

    func settled(_ count: Int) async -> [(String, JSON)] {
        let deadline = Date().addingTimeInterval(2)
        while Date() < deadline {
            let s = lock.withLock { seen }
            if s.count >= count { return s }
            try? await Task.sleep(nanoseconds: 2_000_000)
        }
        return lock.withLock { seen }
    }
}

private let WINDOW: Int64 = 4500

/// Two calls to agents answering `frac` of the inline cap each, routed as one turn
/// of `ai` at a 4.5k window. Returns the delivered message and the cap.
private func twoReplies(_ frac: Double) async throws -> (String, Int64) {
    let probe = Agent(id: AgentId("x"), handlerModule: nil, parentId: nil)
    let cap = AIContext.inlineCap(probe, modelWindow: WINDOW)
    let registry = BundleRegistry()
    registry.register("sized.tools", SizedBundle(chars: Int(Double(cap) * 4 * frac)))
    let sent = Deliveries()
    registry.register("deliveries.tools", sent)
    let kernel = Kernel(storage: .inMemory, bundles: registry)
    let root = Agent(id: AgentId("kernel_state"), handlerModule: nil, parentId: nil)
    kernel.register(root)
    kernel.setRoot(root)
    for (id, module) in [
        ("ai", "deliveries.tools"), ("alpha", "sized.tools"), ("beta", "sized.tools"),
    ] {
        _ = await kernel.send([
            Item(
                AgentId("kernel"),
                .object([
                    "type": .string("create_agent"), "parent_id": .string("kernel_state"),
                    "id": .string(id), "handler_module": .string(module),
                ]))
        ])[0].result
    }
    let text = ["alpha", "beta"].map {
        renderCall(
            "send",
            [
                "target_id": .string($0), "reply_to": .string("self"),
                "payload": .object(["type": .string("ping")]),
            ])
    }.joined()
    await routeReply(
        selfId: AgentId("ai"), kernel: kernel, text: text, clientId: "cli",
        sendTo: nil, systemPrompt: nil, incoming: "", sendTimeout: 180.0, modelWindow: WINDOW)
    let delivered = await sent.settled(1)
    let (_, payload) = try #require(delivered.first)
    return (payload["text"].asString ?? "", cap)
}

@Suite("the inline cap bounds a whole reply batch")
struct BatchCapTests {
    /// Replies each under the cap can together exceed what the next turn fits.
    /// The replies of a batch share the cap: the first fits and is
    /// inline, the one that would push the message past it is held.
    @Test func theInlineCapBoundsTheWholeBatchNotEachReply() async throws {
        let (text, cap) = try await twoReplies(0.6)
        let blocks = text.components(separatedBy: "\n\n")
        #expect(blocks.count == 2, Comment(rawValue: text))
        let a = String(repeating: "a", count: 100)
        let b = String(repeating: "b", count: 100)
        #expect(blocks.first?.contains(a) == true && blocks.first?.contains("NOT inlined") == false)
        #expect(blocks.last?.contains("NOT inlined") == true && blocks.last?.contains(b) == false)
        #expect(
            AIContext.estimateOne(.object(["role": .string("user"), "content": .string(text)]))
                <= cap + 64)
    }

    @Test func repliesThatFitTogetherAreAllInline() async throws {
        let (text, _) = try await twoReplies(0.3)
        #expect(!text.contains("NOT inlined"), Comment(rawValue: text))
        #expect(
            text.contains(String(repeating: "a", count: 100))
                && text.contains(String(repeating: "b", count: 100)))
    }
}
