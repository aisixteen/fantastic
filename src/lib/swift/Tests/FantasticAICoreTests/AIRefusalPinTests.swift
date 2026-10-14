// The AI core's hand-built refusal bodies, byte for byte — key order included.
//
// These replies are spelled as literal dicts (not through `Refusal.make`), and
// several put their machine fields in an order `make` would not (`arg` right
// after `reason`, `request_id` last, no `hint`). Folding them into one helper
// is only safe where the bytes stay the same, so every one is pinned here:
// `watch`/`unwatch` (python `ai_core/core.py` `_watch`, `_unwatch`), the `inquiry` open and
// close, `history {fold}`, and the memory-store fail-fast on `send`.

import FantasticJSON
@testable import FantasticKernel
import Foundation
import OrderedCollections
import Testing

@testable import FantasticAICore
@testable import FantasticRamStore

private func send(_ k: Kernel, _ target: String, _ payload: JSON, as sender: String? = nil) async
    -> String
{
    let item = Item(AgentId(target), payload)
    if let sender {
        return await KernelTaskLocals.$sender.withValue(AgentId(sender)) {
            await k.send([item])[0].result.serialize()
        }
    }
    return await k.send([item])[0].result.serialize()
}

private func pair() async -> Kernel {
    let a = ProviderScript([[doneEnvelope], [doneEnvelope]])
    let b = ProviderScript([[doneEnvelope], [doneEnvelope]])
    let (kernel, _, _) = await bootMockKernel(
        config: scriptedConfig(["alice": a, "bob": b]), backendIds: ["alice", "bob"])
    return kernel
}

@Suite("AI core — hand-built refusal bodies")
struct AIRefusalPinTests {

    @Test func watchAndUnwatchReplies() async {
        let k = await pair()
        #expect(
            await send(k, "alice", ["type": "watch"])
                == #"{"error":"watch: the operator must name a `watcher` inbox","reason":"missing_arg","arg":"watcher"}"#
        )
        #expect(
            await send(k, "alice", ["type": "watch", "watcher": ""])
                == #"{"error":"watch: the operator must name a `watcher` inbox","reason":"missing_arg","arg":"watcher"}"#
        )
        #expect(
            await send(k, "alice", ["type": "watch", "watcher": "ghost"])
                == #"{"error":"watch: 'ghost' has no registered inbox, so nothing would read the row events","#
                + #""reason":"no_consumer","hint":"register the reader first — ctx.ensure_inbox('ghost') — then watch"}"#
        )
        #expect(
            await send(k, "alice", ["type": "watch", "watcher": "carol"], as: "bob")
                == #"{"error":"watch: an agent watches for ITSELF — `watcher` is the operator's","reason":"unauthorized"}"#
        )
        // an agent without an inbox is refused as its own watcher too
        #expect(
            await send(k, "alice", ["type": "watch"], as: "bob")
                == #"{"error":"watch: 'bob' has no registered inbox, so nothing would read the row events","#
                + #""reason":"no_consumer","hint":"register the reader first — ctx.ensure_inbox('bob') — then watch"}"#
        )
        _ = k.ensureInbox(AgentId("probe"))
        #expect(
            await send(k, "alice", ["type": "watch", "watcher": "probe"])
                == #"{"watching":true,"watcher":"probe","body":false}"#)
        #expect(
            await send(k, "alice", ["type": "unwatch", "watcher": "probe"])
                == #"{"watching":false,"watcher":"probe"}"#)
        #expect(
            await send(k, "alice", ["type": "unwatch"]) == #"{"watching":false,"watcher":null}"#)
        // an agent's unwatch names ITSELF, whatever it passes
        #expect(
            await send(k, "alice", ["type": "unwatch", "watcher": "probe"], as: "bob")
                == #"{"watching":false,"watcher":"bob"}"#)
    }

    @Test func historyFoldRefusals() async throws {
        let (k, id) = try await freshMockKernel(
            config: baseConfig { MockProvider(tokens: [doneEnvelope]) })
        #expect(
            await send(k, id.value, ["type": "history", "fold": "x"])
                == #"{"error":"history: `fold` must be a fold id (int)","reason":"bad_arg","arg":"fold"}"#
        )
        #expect(
            await send(k, id.value, ["type": "history", "fold": 424242])
                == #"{"error":"history: 424242 is not a fold in your conversation","reason":"not_found","arg":"fold"}"#
        )
    }

    @Test func inquiryTextRefusals() async throws {
        let (k, id) = try await freshMockKernel(
            config: baseConfig {
                MockProvider(tokens: ["I would pick b. " + doneEnvelope])
            })
        let empty =
            #"{"error":"inquiry: `text` must be a non-empty string","reason":"missing_arg","arg":"text"}"#
        #expect(await send(k, id.value, ["type": "inquiry"]) == empty)
        #expect(await send(k, id.value, ["type": "inquiry", "text": " \n "]) == empty)
        // Anything else is ACCEPTED at once (fully async); a run
        // that ends without `inquiry_answer` is told to the asker later
        // (`InquiryAsyncTests`).
        let accepted = await send(k, id.value, ["type": "inquiry", "text": "a or b?", "request_id": "r-1"])
        #expect(accepted.hasPrefix(#"{"accepted":true,"re":"#) && accepted.hasSuffix(#","request_id":"r-1"}"#), "\(accepted)")
    }

    /// The store refused (here: the record names no agent): no turn runs, and
    /// the reply says so with `reason: memory_store` and the caller's client.
    @Test func aSendWhoseStoreFailsFailsFast() async throws {
        let (k, ids, _) = await bootMockKernel(
            config: baseConfig { MockProvider(tokens: [doneEnvelope]) },
            backendIds: ["lost"], meta: ["memory_store_id": "nowhere"])
        let out = await k.send([
            Item(ids["lost"]!, .object(["type": "send", "text": "hi", "client_id": "c"]))
        ])[0].result
        #expect(
            Array(out.asObject?.keys ?? []) == ["error", "reason", "client_id"],
            "\(out.serialize())")
        #expect(out["reason"] == "memory_store")
        #expect(out["client_id"] == "c")
        #expect(out["error"].asString?.isEmpty == false)
    }
}
