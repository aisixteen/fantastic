// A docs body is HELD under its pid this run: it opens with the
// line saying it is erased after the next message and how to keep what is needed,
// the model's own `grep` on that pid returns the verb line, and once answered the
// body is cut to a stub naming the same grep. Python `test_reply_manifest.py`.

import FantasticJSON
import FantasticKernel
import Foundation
import OrderedCollections
import Testing

@testable import FantasticAICore

@Suite("docs held for grep")
struct DocsHeldTests {

    @Test func anAnsweredDocsReplyIsCutToAStubNamingItsHeldPid() {
        let body =
            "[reply to 12 · kernel docs]\n<of>runner</of>\n<bundle>exec_bridge.tools</bundle>"
            + "\n<tier>oneliners</tier>\n<verbs>\n<start>args: command:str</start>\n</verbs>"
        let sibling = "[reply to 13 · notes read]\nline one"
        let hist: [JSON] = [
            .object(["role": "user", "content": .string(body + "\n\n" + sibling), "id": 7]),
            .object(["role": "assistant", "content": "<call …>", "id": 8]),
        ]
        let blocks = (dropReadDocs(hist)[0]["content"].asString ?? "").components(separatedBy: "\n\n")
        #expect(
            blocks[0]
                == "[reply to 12 · kernel docs]\n[docs of=runner tier=oneliners — dropped from your view; "
                + "held this run: send yourself <type>grep</type> <ids><item>12</item></ids> "
                + "<query>start</query>]")
        #expect(blocks[1] == sibling)
    }

    @Test func theGrepExampleIsARealVerbAndAVerblessBodyIsNotCut() {
        #expect(
            exampleVerb("<of>x</of>\n<verbs>\n<reflect>a</reflect>\n<boot>b</boot>\n<write>c\nmore</write>\n</verbs>")
                == "write")
        #expect(exampleVerb("<of>x</of>\n<verbs>\n<reflect>a</reflect>\n</verbs>") == "reflect")
        #expect(exampleVerb("<of>x</of>\n<verbs>\n</verbs>") == nil)
        let bare: JSON = .string(
            "[reply to 5 · kernel docs]\n<of>x</of>\n<bundle>x.tools</bundle>\n<tier>oneliners</tier>\n<verbs>\n</verbs>")
        let hist: [JSON] = [
            .object(["role": "user", "content": bare]), .object(["role": "assistant", "content": "<call …>"]),
        ]
        #expect(dropReadDocs(hist) == hist)
    }

    @Test func aDocsBodyIsHeldAndItsFirstLineNamesTheGrep() async throws {
        let mind = Mind()
        var cfg = scriptedConfig([:])
        cfg.makeProvider = { _, _, _ in ReactiveProvider(mind) }
        let (k, ids, _) = await bootMockKernel(config: cfg, backendIds: ["rm_docs"])
        let ai = ids["rm_docs"]!
        var cc = k.agent(ai)?.metaValue(forKey: "can_call")?.asObject ?? [:]
        cc["kernel_state"] = .array(["docs"])
        k.update(ai, ["can_call": .object(cc)])
        var cb = k.agent(AgentId("kernel_state"))?.metaValue(forKey: "callable_by")?.asObject ?? [:]
        cb["rm_docs"] = .array(["docs"])
        k.update(AgentId("kernel_state"), ["callable_by": .object(cb)])
        _ = await k.send([Item(ai, .object(["type": "send", "text": "go"]))])
        for _ in 0..<300 where mind.seen.count < 3 { try await Task.sleep(nanoseconds: 10_000_000) }
        let seen = mind.seen
        #expect(seen.count >= 3, "\(seen)")
        let lines = seen[1].components(separatedBy: "\n")
        #expect(lines.count > 1 && lines[1].hasPrefix("[erased from your view after your next message"), "\(seen[1])")
        #expect(lines.count > 1 && !lines[1].contains("<query>verb</query>"), "\(seen[1])")
        #expect(seen[2].contains("rm_docs grep") && seen[2].contains("ack_deadletter"), "\(seen[2])")
    }
}

/// A scripted mind that writes its turn from what it RECEIVED: docs first, then a
/// grep on the pid the docs head named, then `done`.
private final class Mind: @unchecked Sendable {
    private let lock = NSLock()
    private var got: [String] = []
    var seen: [String] { lock.withLock { got } }

    func turn(_ last: String) -> String {
        let n = lock.withLock { () -> Int in
            got.append(last)
            return got.count
        }
        switch n {
        case 1:
            return renderCall(
                "send", ["target_id": "kernel", "payload": .object(["type": "docs", "of": "kernel"])])
        case 2:
            let pid = last.range(of: #"<ids><item>(\d+)</item></ids>"#, options: .regularExpression)
                .map { String(last[$0]).filter(\.isNumber) } ?? "0"
            return renderCall(
                "send",
                [
                    "target_id": "rm_docs",
                    "payload": .object([
                        "type": "grep", "query": "ack_deadletter", "ids": .array([.integer(Int64(pid)!)]),
                    ]),
                ])
        default:
            return "<call name=\"done\"></call>"
        }
    }
}

private struct ReactiveProvider: AIProvider {
    let mind: Mind
    init(_ mind: Mind) { self.mind = mind }
    var model: String { "mock-1" }
    func chat(messages: [JSON]) -> AsyncThrowingStream<AIChunk, Error> {
        let text = mind.turn(messages.last?["content"].asString ?? "")
        return AsyncThrowingStream { c in
            c.yield(.token(text))
            c.finish()
        }
    }
    func chat(messages: [JSON], maxTokens: Int?) -> AsyncThrowingStream<AIChunk, Error> {
        chat(messages: messages)
    }
}
