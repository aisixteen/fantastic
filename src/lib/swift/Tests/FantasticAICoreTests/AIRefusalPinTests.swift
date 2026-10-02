// The AI core's hand-built refusal bodies, byte for byte — key order included.
//
// These replies are spelled as literal dicts (not through `Refusal.make`), and
// several put their machine fields in an order `make` would not (`arg` right
// after `reason`, `request_id` last, no `hint`). Folding them into one helper
// is only safe where the bytes stay the same, so every one is pinned here:
// `fork_from`, `request_share`, `watch`/`unwatch` (python `ai_core/core.py`
// `_fork_from`, `_request_share`, `_watch`, `_unwatch`), the `inquiry` open and
// close, `history {fold}`, and the memory-store fail-fast on `send`.

import FantasticJSON
import FantasticKernel
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
        return await KernelTaskLocals.$currentSender.withValue(AgentId(sender)) {
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

    @Test func forkFromRefusals() async throws {
        let k = await pair()
        func fork(_ extra: OrderedDictionary<String, JSON>) async -> String {
            var p: OrderedDictionary<String, JSON> = ["type": "fork_from"]
            for (key, v) in extra { p[key] = v }
            return await send(k, "bob", .object(p))
        }
        let noOwner =
            #"{"error":"fork_from: `owner` is required","reason":"missing_arg","arg":"owner"}"#
        #expect(await fork(["message": "go", "grant": 1]) == noOwner)
        #expect(await fork(["owner": "  ", "message": "go", "grant": 1]) == noOwner)
        #expect(await fork(["owner": 5, "message": "go", "grant": 1]) == noOwner)
        #expect(
            await fork(["owner": "alice", "grant": 1])
                == #"{"error":"fork_from: `message` is required","reason":"missing_arg","arg":"message"}"#
        )
        let noGrant =
            #"{"error":"fork_from: `grant` (an int) is required","reason":"missing_arg","arg":"grant"}"#
        #expect(await fork(["owner": "alice", "message": "go"]) == noGrant)
        #expect(await fork(["owner": "alice", "message": "go", "grant": "x"]) == noGrant)
        #expect(await fork(["owner": "alice", "message": "go", "grant": 0]) == noGrant)
        #expect(
            await fork(["owner": "alice", "message": "go", "grant": 99999])
                == #"{"error":"fork_from: no grant 99999 was given to you","reason":"not_found","arg":"grant"}"#
        )

        try await saveHistory(
            "alice", k, [.object(["role": .string("user"), "content": .string("x")])])
        let g = await KernelTaskLocals.$currentSender.withValue(AgentId("alice")) {
            await k.send([
                Item(
                    AgentId("memory"),
                    .object([
                        "type": .string("share"), "with": .string("bob"),
                        "scope": .object(["head_until": .integer(1_000_000_000)]),
                    ]))
            ])[0].result
        }
        let gid = g["grant"].asInt ?? -1
        #expect(
            await fork(["owner": "mallory", "message": "go", "grant": .integer(gid)])
                == #"{"error":"fork_from: grant \#(gid) was given by alice, not mallory","reason":"bad_arg","arg":"owner"}"#
        )
    }

    @Test func requestShareNeedsARowId() async {
        let k = await pair()
        let want =
            #"{"error":"request_share: `row` (an int) is required","reason":"missing_arg","arg":"row"}"#
        #expect(await send(k, "alice", ["type": "request_share"], as: "bob") == want)
        #expect(await send(k, "alice", ["type": "request_share", "row": "x"], as: "bob") == want)
    }

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

    @Test func inquiryOpenAndCloseRefusals() async throws {
        let (k, id) = try await freshMockKernel(
            config: baseConfig {
                MockProvider(tokens: ["I would pick b. " + doneEnvelope])
            })
        let empty =
            #"{"error":"inquiry: `text` must be a non-empty string","reason":"missing_arg","arg":"text"}"#
        #expect(await send(k, id.value, ["type": "inquiry"]) == empty)
        #expect(await send(k, id.value, ["type": "inquiry", "text": " \n "]) == empty)
        // a turn that never emits `inquiry_answer` is told so — its prose is not
        // harvested as the answer
        #expect(
            await send(k, id.value, ["type": "inquiry", "text": "a or b?", "request_id": "r-1"])
                == #"{"error":"you were sent an inquiry and did not answer it: emit "#
                + #"<call name=\"inquiry_answer\"><answer>what you concluded</answer></call>","#
                + #""reason":"unhandled","request_id":"r-1"}"#)
        #expect(
            await send(k, id.value, ["type": "inquiry", "text": "a or b?"])
                == #"{"error":"you were sent an inquiry and did not answer it: emit "#
                + #"<call name=\"inquiry_answer\"><answer>what you concluded</answer></call>","#
                + #""reason":"unhandled","request_id":null}"#)
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
